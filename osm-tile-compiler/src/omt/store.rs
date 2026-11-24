//! Bounded external tile-record sort and OpenMapTiles MVT archive publication.
use super::merge_cache::{self, LayerEntry, MergeCache, OldTile, TileEntry};
#[cfg(test)]
use super::record_codec::encode_payload;
use super::record_codec::{
    LAYER_NAMES, Reader, decode_payload, encode_attrs, encode_payload_head, encode_varint,
    layer_id, zigzag,
};
use super::render::Record;
use super::spill::{RunReader, RunWriter};
use crate::control::Control;
use crate::{
    ArchiveCenter, CompileBounds, TileFormat,
    archive::ArchiveOptions,
    output::{ArchiveSpec, ArchiveTarget},
    tile_geometry::{TileGeometry, TilePoint, TileRingGroup},
    tile_stats::{TILE_WARNING_BYTES, TileStats, TileStatsBuilder},
};
use fast_mvt::{MvtLayerBuilder, MvtValueRef};
use geo::algorithm::buffer::{BufferStyle, LineJoin};
use geo::coordinate_position::CoordPos;
use geo::dimensions::Dimensions;
use geo::{Area, BoundingRect, Buffer, Distance, Euclidean, Intersects, Relate, unary_union};
use geo_types::{Coord, LineString, MultiPolygon, Polygon, Rect};
use serde::{Deserialize, Serialize};
use std::cmp::{Ordering, Reverse};
use std::collections::binary_heap::PeekMut;
use std::collections::{BTreeMap, BinaryHeap};
use std::fs::{self, File};
use std::io::Write;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering as AtomicOrdering};
use std::sync::{Arc, Mutex};

const CHUNK_BYTES: usize = 64 * 1024 * 1024;
pub(super) const MAX_RECORD_BYTES: usize = 64 * 1024 * 1024;
const MAX_TILE_BYTES: usize = 256 * 1024 * 1024;
pub(super) const MERGE_FAN_IN: usize = 128;
/// Blocks of each run the final merge's decoders keep decoded ahead of the
/// merge, counting one being decoded.
const BLOCKS_AHEAD: usize = 2;

/// An attribute name: the profile's names are static, so records decoded at
/// encode time borrow them instead of allocating a string per attribute.
pub(super) type AttrKey = std::borrow::Cow<'static, str>;

/// A record's attributes, sorted by name: a `BTreeMap`'s order and lookups
/// over one vector, which a decoded record fills with a single allocation.
#[derive(Debug, Clone, Default, PartialEq)]
pub(super) struct AttrMap(Vec<(AttrKey, AttrValue)>);

impl AttrMap {
    #[cfg(test)]
    pub(super) fn new() -> Self {
        Self(Vec::new())
    }
    fn find(&self, key: &str) -> Result<usize, usize> {
        self.0.binary_search_by(|(k, _)| (**k).cmp(key))
    }
    pub(super) fn get(&self, key: &str) -> Option<&AttrValue> {
        self.find(key).ok().map(|i| &self.0[i].1)
    }
    pub(super) fn contains_key(&self, key: &str) -> bool {
        self.find(key).is_ok()
    }
    /// Sets `key` to `value` and returns the value it replaced.
    pub(super) fn insert(&mut self, key: AttrKey, value: AttrValue) -> Option<AttrValue> {
        match self.find(&key) {
            Ok(i) => Some(std::mem::replace(&mut self.0[i].1, value)),
            Err(i) => {
                self.0.insert(i, (key, value));
                None
            }
        }
    }
    pub(super) fn remove(&mut self, key: &str) -> Option<AttrValue> {
        self.find(key).ok().map(|i| self.0.remove(i).1)
    }
    pub(super) fn retain(&mut self, mut keep: impl FnMut(&AttrKey, &mut AttrValue) -> bool) {
        self.0.retain_mut(|(k, v)| keep(k, v));
    }
    pub(super) fn iter(&self) -> AttrIter<'_> {
        self.0.iter().map(|(k, v)| (k, v))
    }
    #[cfg(test)]
    pub(super) fn clear(&mut self) {
        self.0.clear();
    }
    pub(super) fn len(&self) -> usize {
        self.0.len()
    }
}

type AttrIter<'a> = std::iter::Map<
    std::slice::Iter<'a, (AttrKey, AttrValue)>,
    fn(&'a (AttrKey, AttrValue)) -> (&'a AttrKey, &'a AttrValue),
>;

impl std::ops::Index<&str> for AttrMap {
    type Output = AttrValue;
    fn index(&self, key: &str) -> &AttrValue {
        self.get(key).expect("no attribute of that name")
    }
}

impl<'a> IntoIterator for &'a AttrMap {
    type Item = (&'a AttrKey, &'a AttrValue);
    type IntoIter = AttrIter<'a>;
    fn into_iter(self) -> Self::IntoIter {
        self.iter()
    }
}

impl IntoIterator for AttrMap {
    type Item = (AttrKey, AttrValue);
    type IntoIter = std::vec::IntoIter<(AttrKey, AttrValue)>;
    fn into_iter(self) -> Self::IntoIter {
        self.0.into_iter()
    }
}

impl FromIterator<(AttrKey, AttrValue)> for AttrMap {
    /// As `BTreeMap` collects: sorted by key, the last value of a repeated
    /// key kept.
    fn from_iter<I: IntoIterator<Item = (AttrKey, AttrValue)>>(iter: I) -> Self {
        let mut entries: Vec<_> = iter.into_iter().collect();
        if !entries.is_sorted_by(|a, b| a.0 < b.0) {
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut deduped: Vec<(AttrKey, AttrValue)> = Vec::with_capacity(entries.len());
            for entry in entries {
                match deduped.last_mut() {
                    Some(last) if last.0 == entry.0 => *last = entry,
                    _ => deduped.push(entry),
                }
            }
            entries = deduped;
        }
        Self(entries)
    }
}

impl<const N: usize> From<[(AttrKey, AttrValue); N]> for AttrMap {
    fn from(entries: [(AttrKey, AttrValue); N]) -> Self {
        entries.into_iter().collect()
    }
}

impl Serialize for AttrMap {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_map(self.iter())
    }
}

impl<'de> Deserialize<'de> for AttrMap {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        BTreeMap::<AttrKey, AttrValue>::deserialize(deserializer)
            .map(|map| map.into_iter().collect())
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub(super) enum AttrValue {
    String(String),
    Int(i64),
    UInt(u64),
    Double(f64),
    Bool(bool),
}

impl AttrValue {
    fn mvt(&self) -> MvtValueRef<'_> {
        match self {
            Self::String(v) => MvtValueRef::String(v),
            Self::Int(v) => MvtValueRef::SInt(*v),
            Self::UInt(v) => MvtValueRef::UInt(*v),
            Self::Double(v) => MvtValueRef::Double(*v),
            Self::Bool(v) => MvtValueRef::Bool(*v),
        }
    }
}

/// What an archive says besides its tiles: its header fields and metadata.
pub(super) struct ArchiveHeader<'a> {
    pub bounds: CompileBounds,
    pub center: ArchiveCenter,
    pub options: &'a ArchiveOptions,
    pub min_zoom: u8,
    pub max_zoom: u8,
    /// The layers the metadata declares: the profile's in `LAYERS` order,
    /// then the data layers.
    pub layers: &'a [&'a str],
    /// The ids of the supplemental datasets left out, for the metadata.
    pub omitted_sources: &'a [&'static str],
    /// How current the OSM data is, for the metadata.
    pub osm_data: &'a super::OsmDataInfo,
    /// The metadata's `data_layers`: what each data layer was read from.
    pub data_layers: &'a [serde_json::Value],
}

#[cfg(test)]
impl ArchiveHeader<'static> {
    /// Default options and a center at 0 0, for tests.
    pub fn for_test(bounds: CompileBounds, min_zoom: u8, max_zoom: u8) -> Self {
        static OPTIONS: std::sync::LazyLock<ArchiveOptions> =
            std::sync::LazyLock::new(ArchiveOptions::default);
        static OSM_DATA: super::OsmDataInfo = super::OsmDataInfo {
            replication_timestamp: None,
            replication_sequence_number: None,
            replication_base_url: None,
        };
        Self {
            bounds,
            center: ArchiveCenter {
                longitude: 0.0,
                latitude: 0.0,
                zoom: min_zoom,
            },
            options: &OPTIONS,
            min_zoom,
            max_zoom,
            layers: &super::LAYERS,
            omitted_sources: &[],
            osm_data: &OSM_DATA,
            data_layers: &[],
        }
    }
}

pub(super) struct Store {
    directory: PathBuf,
    /// Locked while this compile uses the scratch directory (see `new`).
    _lock: File,
    runs: Vec<PathBuf>,
    /// Uncompressed bytes the sorters spilled, for the merge's progress.
    spilled: Arc<AtomicU64>,
    sequence: usize,
    fields: BTreeMap<String, BTreeMap<String, String>>,
    zooms: BTreeMap<String, (u8, u8)>,
    /// Set once the runs are checkpointed (`checkpoint`): the merge then
    /// records each of its passes, and the runs stay when the compile fails.
    checkpoints: Option<crate::resume::Checkpoints>,
}

/// Global insertion order of a record: the profile source's ordinal, the
/// feature's position in that source's output, then the record's position in
/// that feature's emission. It is the last `SortKey` component, so the merged
/// order equals serial insertion order however records were rendered, spilled,
/// or distributed across workers. Source ordinals only need to be unique and
/// ordered, not dense, so workers derive them without a shared counter.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct Seq {
    pub source: u64,
    pub feature: u32,
    pub record: u32,
}

/// Order-independent metadata of the records one worker rendered. Sequence
/// numbers recover the one order-sensitive rule: a synthesized rank default
/// counts only when it precedes every explicit `rank` attribute.
#[derive(Default)]
pub(super) struct MetadataSummary {
    fields: BTreeMap<AttrKey, BTreeMap<AttrKey, FieldSummary>>,
    zooms: BTreeMap<AttrKey, (u8, u8)>,
}
#[derive(Default)]
struct FieldSummary {
    default_at: Option<Seq>,
    explicit: Option<&'static str>,
    explicit_at: Option<Seq>,
}
fn earliest(a: Option<Seq>, b: Option<Seq>) -> Option<Seq> {
    a.into_iter().chain(b).min()
}
fn merge_kind(a: Option<&'static str>, b: Option<&'static str>) -> Option<&'static str> {
    match (a, b) {
        (Some(a), Some(b)) if a != b => Some("String"),
        (a, b) => a.or(b),
    }
}
impl MetadataSummary {
    pub(super) fn observe(&mut self, record: &Record, seq: Seq) {
        self.zooms
            .entry(record.layer.clone())
            .and_modify(|(min, max)| {
                *min = (*min).min(record.zoom);
                *max = (*max).max(record.zoom);
            })
            .or_insert((record.zoom, record.zoom));
        let fields = self.fields.entry(record.layer.clone()).or_default();
        for (key, value) in &record.attrs {
            if key.starts_with('_') {
                continue;
            }
            let kind = match value {
                AttrValue::String(_) => "String",
                AttrValue::Bool(_) => "Boolean",
                _ => "Number",
            };
            let field = fields.entry(key.clone()).or_default();
            field.explicit = merge_kind(field.explicit, Some(kind));
            field.explicit_at = earliest(field.explicit_at, Some(seq));
        }
        if record.rank {
            let field = fields.entry("rank".into()).or_default();
            field.default_at = earliest(field.default_at, Some(seq));
        }
    }

    pub(super) fn merge(&mut self, other: MetadataSummary) {
        for (layer, (min, max)) in other.zooms {
            self.zooms
                .entry(layer)
                .and_modify(|range| {
                    range.0 = range.0.min(min);
                    range.1 = range.1.max(max);
                })
                .or_insert((min, max));
        }
        for (layer, fields) in other.fields {
            let target = self.fields.entry(layer).or_default();
            for (key, field) in fields {
                let target = target.entry(key).or_default();
                target.default_at = earliest(target.default_at, field.default_at);
                target.explicit = merge_kind(target.explicit, field.explicit);
                target.explicit_at = earliest(target.explicit_at, field.explicit_at);
            }
        }
    }

    pub(super) fn apply(self, store: &mut Store) {
        for (layer, (min, max)) in self.zooms {
            store
                .zooms
                .entry(layer.into_owned())
                .and_modify(|range| {
                    range.0 = range.0.min(min);
                    range.1 = range.1.max(max);
                })
                .or_insert((min, max));
        }
        for (layer, fields) in self.fields {
            let target = store.fields.entry(layer.into_owned()).or_default();
            for (key, field) in fields {
                let mut kind = field.explicit;
                if field
                    .default_at
                    .is_some_and(|d| field.explicit_at.is_none_or(|e| d < e))
                {
                    kind = merge_kind(Some("Number"), kind);
                }
                if let Some(kind) = kind {
                    target
                        .entry(key.into_owned())
                        .and_modify(|old| {
                            if old != kind {
                                *old = "String".into();
                            }
                        })
                        .or_insert_with(|| kind.into());
                }
            }
        }
    }
}

fn order(a: &Record, b: &Record) -> Ordering {
    a.tile_id
        .cmp(&b.tile_id)
        .then_with(|| a.layer.cmp(&b.layer))
        .then_with(|| a.sort_key.cmp(&b.sort_key))
        .then_with(|| a.id.cmp(&b.id))
}

// A record's size, for the sort budget and the merge's progress, is that of a
// frame: a magic byte, the header length, the header, then the payload. The
// header holds the payload length and the sort key as varints. Run files
// store records differently (see `spill`).
pub(super) const LAYER_VERBATIM: u8 = 0xFF;
pub(super) const MAX_LAYER_BYTES: usize = 128;
const MAX_HEADER_BYTES: usize = 255;

/// A layer name. The 16 OMT layers are table indices that sort like their
/// names, so comparing two of them is a byte comparison; any other name is
/// kept as text and compared as text.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum Layer {
    Known(u8),
    Other(Box<str>),
}
impl Layer {
    pub(super) fn new(name: &str) -> Self {
        layer_id(name).map_or_else(|| Self::Other(name.into()), Self::Known)
    }
    fn name(&self) -> &str {
        match self {
            Self::Known(id) => LAYER_NAMES[usize::from(*id)],
            Self::Other(name) => name,
        }
    }
    fn into_name(self) -> AttrKey {
        match self {
            Self::Known(id) => AttrKey::Borrowed(LAYER_NAMES[usize::from(id)]),
            Self::Other(name) => AttrKey::Owned(name.into_string()),
        }
    }
}
impl Ord for Layer {
    fn cmp(&self, other: &Self) -> Ordering {
        match (self, other) {
            (Self::Known(a), Self::Known(b)) => a.cmp(b),
            _ => self.name().cmp(other.name()),
        }
    }
}
impl PartialOrd for Layer {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub(super) struct SortKey {
    pub tile_id: u64,
    pub layer: Layer,
    pub sort_key: i64,
    pub id: u64,
    pub seq: Seq,
}
impl SortKey {
    fn of(record: &Record, seq: Seq) -> Self {
        Self {
            tile_id: record.tile_id,
            layer: Layer::new(&record.layer),
            sort_key: record.sort_key,
            id: record.id,
            seq,
        }
    }
}
pub(super) struct EncodedRecord {
    pub key: SortKey,
    pub payload: Vec<u8>,
}
thread_local! {
    // Payloads are built here, then copied out at their exact size.
    static PAYLOAD_SCRATCH: std::cell::RefCell<Vec<u8>> = const { std::cell::RefCell::new(Vec::new()) };
}
impl EncodedRecord {
    #[cfg(test)]
    pub(super) fn encode(record: &Record, seq: Seq) -> Result<Self, String> {
        Self::encode_with(record, seq, |scratch| encode_payload(record, scratch))
    }
    /// `encode`, with the record's attributes already encoded as `attrs`
    /// (by `encode_attrs`), as records that share them can be.
    pub(super) fn encode_with_attrs(
        record: &Record,
        seq: Seq,
        attrs: &[u8],
    ) -> Result<Self, String> {
        debug_assert_eq!(
            attrs,
            &{
                let mut own = Vec::new();
                encode_attrs(&record.attrs, &mut own);
                own
            }[..]
        );
        Self::encode_with(record, seq, |scratch| {
            encode_payload_head(record, scratch);
            scratch.extend_from_slice(attrs);
        })
    }
    fn encode_with(
        record: &Record,
        seq: Seq,
        encode: impl FnOnce(&mut Vec<u8>),
    ) -> Result<Self, String> {
        if layer_id(&record.layer).is_none() && record.layer.len() > MAX_LAYER_BYTES {
            return Err("OMT sort layer exceeds frame limit".into());
        }
        let payload = PAYLOAD_SCRATCH.with_borrow_mut(|scratch| {
            scratch.clear();
            encode(scratch);
            scratch.clone()
        });
        if payload.len() > MAX_RECORD_BYTES {
            return Err("OMT record exceeds bounded record limit".into());
        }
        Ok(Self {
            key: SortKey::of(record, seq),
            payload,
        })
    }
    pub(super) fn decode(self) -> Result<Record, String> {
        decode_payload(
            &self.payload,
            self.key.tile_id,
            self.key.layer.into_name(),
            self.key.sort_key,
            self.key.id,
        )
    }
    /// The frame header after the magic and length bytes.
    fn header(&self) -> Header {
        Header::of(&self.key, self.payload.len())
    }
    pub(super) fn frame_bytes(&self) -> usize {
        frame_bytes(&self.key, self.payload.len())
    }
}
/// The size of a record as a frame: a magic byte, the header length, the
/// header, then the payload.
pub(super) fn frame_bytes(key: &SortKey, payload: usize) -> usize {
    2 + Header::of(key, payload).len + payload
}
impl Header {
    fn of(key: &SortKey, payload: usize) -> Self {
        let mut header = Header::default();
        header.varint(payload as u64);
        header.varint(key.tile_id);
        match &key.layer {
            Layer::Known(id) => header.bytes(&[*id]),
            Layer::Other(name) => {
                header.bytes(&[LAYER_VERBATIM]);
                header.varint(name.len() as u64);
                header.bytes(name.as_bytes());
            }
        }
        header.zigzag(key.sort_key);
        header.varint(key.id);
        header.varint(key.seq.source);
        header.varint(u64::from(key.seq.feature));
        header.varint(u64::from(key.seq.record));
        header
    }
}
struct Header {
    bytes: [u8; MAX_HEADER_BYTES],
    len: usize,
}
impl Default for Header {
    fn default() -> Self {
        Self {
            bytes: [0; MAX_HEADER_BYTES],
            len: 0,
        }
    }
}
impl Header {
    // The longest header (layers are capped at MAX_LAYER_BYTES) fits.
    fn bytes(&mut self, bytes: &[u8]) {
        self.bytes[self.len..self.len + bytes.len()].copy_from_slice(bytes);
        self.len += bytes.len();
    }
    fn varint(&mut self, value: u64) {
        let mut bytes = [0; 10];
        let len = encode_varint(value, &mut bytes);
        self.bytes(&bytes[..len]);
    }
    fn zigzag(&mut self, value: i64) {
        self.varint(zigzag(value));
    }
}
/// One worker's in-memory run buffer. Full buffers are sorted and spilled to
/// run files by the owning thread; unfinished runs are deleted on drop.
///
/// Records are kept as frames (the header length, the header `EncodedRecord`
/// writes, then the payload), one after another in `frames`, with a 32-byte
/// sort entry each: a record costs its frame and its entry, where an
/// `EncodedRecord` costs 80 bytes and a heap block for its payload besides.
pub(super) struct Sorter {
    directory: PathBuf,
    worker: usize,
    /// Frame bytes this buffer holds before it spills; see `SortBudget`.
    budget: usize,
    limits: Arc<SortBudget>,
    frames: Vec<u8>,
    entries: Vec<SortEntry>,
    runs: Vec<PathBuf>,
    /// Frame bytes and records this sorter has spilled.
    spilled_frames: u64,
    spilled_records: u64,
    spilled: Arc<AtomicU64>,
}

// The largest frame buffer a worker grows to. Frame offsets are 32-bit, and
// one sort of more than this takes long enough to stall the worker's share
// of the rendering noticeably.
const MAX_FRAME_BUDGET: usize = 2 << 30;
// Without an estimate of the compile's size, the workers keep their starting
// buffers until they have spilled this many runs between them. Below it the
// final merge takes every run in one pass, so larger buffers would only add
// memory; above it the compile is headed for more runs than one merge takes.
const GROW_AFTER_RUNS: usize = MERGE_FAN_IN / 2;
// With an estimate, the workers aim for this many runs in all, leaving the
// rest of the final merge's fan-in for each worker's last, partial run and for
// an estimate that falls short.
const TARGET_RUNS: usize = MERGE_FAN_IN * 3 / 4;
// With an estimate, buffers keep the floor until this many runs, so that a
// compile whose estimate is far too large (a small area of a large extract)
// spills a few runs before any buffer grows.
const PLAN_AFTER_RUNS: usize = MERGE_FAN_IN / 8;

/// How large the workers' sort buffers may grow.
///
/// Every worker starts with `floor` frame bytes. Given an estimate of the
/// frame bytes the whole compile spills, a buffer grows only as far as it
/// must for the compile to end near `TARGET_RUNS` runs: the estimated bytes
/// still to come, spread over the runs still to spill. Near and past the
/// estimate, at least a quarter of what was spilled is taken to be still to
/// come. So the buffers grow with the compile's
/// size divided by the merge's fan-in, not with the compile's size: a
/// country's buffers stay at tens of MiB where they used to grow to half of
/// what the workers had spilled.
///
/// Without an estimate, once the workers have spilled `GROW_AFTER_RUNS`
/// runs between them, each worker's next buffer is as large as everything it
/// has spilled so far.
///
/// Either way a buffer is never more than doubled at once, nor larger than
/// its share of the sort memory. A small extract keeps the floor, its run
/// count and its peak memory; a large one ends with about `TARGET_RUNS`
/// runs, or, when memory caps the buffers, more, and the merge combines the
/// small early runs first (`Store::finish`), rather than every record twice
/// or more.
#[derive(Debug)]
pub(super) struct SortBudget {
    floor: usize,
    /// Bytes each worker may hold in frames and sort entries together.
    worker_memory: usize,
    /// Frame bytes the whole compile is expected to spill.
    expected: Option<u64>,
    runs: AtomicUsize,
    /// Frame bytes all workers have spilled.
    spilled: AtomicU64,
}

impl SortBudget {
    /// `floor` frame bytes per worker, growing as far as `memory` bytes split
    /// among `workers`; `memory` below the floors keeps the floor.
    pub(super) fn new(floor: usize, memory: Option<u64>, workers: usize) -> Self {
        let worker_memory = memory.map_or(0, |memory| {
            usize::try_from(memory / workers.max(1) as u64).unwrap_or(usize::MAX)
        });
        Self {
            floor: floor.max(1),
            worker_memory,
            expected: None,
            runs: AtomicUsize::new(0),
            spilled: AtomicU64::new(0),
        }
    }
    /// Sizes the buffers for a compile expected to spill about `frames`
    /// frame bytes in all.
    pub(super) fn expecting(mut self, frames: Option<u64>) -> Self {
        self.expected = frames;
        self
    }
    /// A budget that stays at `bytes`.
    #[cfg(test)]
    pub(super) fn fixed(bytes: usize) -> Arc<Self> {
        Arc::new(Self::new(bytes, None, 1))
    }

    /// The frame budget for a sorter's next run, after it spilled a run of
    /// `run_frames` on a budget of `current` (`spilled_*` count all of the
    /// sorter's runs).
    fn after_spill(
        &self,
        current: usize,
        run_frames: u64,
        spilled_frames: u64,
        spilled_records: u64,
    ) -> usize {
        let runs = self.runs.fetch_add(1, AtomicOrdering::Relaxed) + 1;
        let total = self.spilled.fetch_add(run_frames, AtomicOrdering::Relaxed) + run_frames;
        let growing = match self.expected {
            Some(_) => runs >= PLAN_AFTER_RUNS,
            None => runs >= GROW_AFTER_RUNS,
        };
        if !growing || spilled_frames == 0 {
            return current;
        }
        // A record costs its frame and its entry, and the entries' vector
        // may hold as much spare capacity again once it has doubled.
        let entry_bytes = 2 * spilled_records as u128 * size_of::<SortEntry>() as u128;
        let frames = spilled_frames as u128;
        let ceiling = self.worker_memory as u128 * frames / (frames + entry_bytes);
        let ceiling = (ceiling.min(MAX_FRAME_BUDGET as u128) as usize).max(self.floor);
        let doubled = usize::try_from(spilled_frames).unwrap_or(usize::MAX);
        let Some(expected) = self.expected else {
            return doubled.max(current).min(ceiling);
        };
        // However far past the estimate, a quarter more is still to come.
        let expected = expected.max(total.saturating_add(total / 4));
        let slots = TARGET_RUNS.saturating_sub(runs).max(1) as u64;
        let needed = usize::try_from((expected - total).div_ceil(slots)).unwrap_or(usize::MAX);
        if needed <= current {
            return current;
        }
        needed.min(doubled.max(current)).min(ceiling)
    }
}

/// A buffered record's sort key, less what the frame at `offset` holds: the
/// name of a layer outside the table, and the sequence, which decides only
/// between records equal in the rest.
#[derive(Clone, Copy)]
struct SortEntry {
    tile_id: u64,
    sort_key: i64,
    id: u64,
    offset: u32,
    /// The layer's table index, or `LAYER_VERBATIM` for a named layer.
    layer: u8,
}

/// A buffered frame, read back.
struct Frame<'a> {
    layer: Option<&'a str>,
    seq: Seq,
    payload: &'a [u8],
}

impl Frame<'_> {
    /// Reads the frame at the start of `frames`, which `Sorter::add` wrote.
    fn at(frames: &[u8]) -> Frame<'_> {
        let parse = || -> Result<Frame<'_>, String> {
            let mut reader = Reader::new(frames);
            let header = reader.u8()?;
            let mut header = Reader::new(reader.take(usize::from(header))?);
            let payload = usize::try_from(header.varint()?).map_err(|e| e.to_string())?;
            header.varint()?;
            let layer = match header.u8()? {
                LAYER_VERBATIM => Some(header.str()?),
                _ => None,
            };
            header.zigzag()?;
            header.varint()?;
            let seq = Seq {
                source: header.varint()?,
                feature: u32::try_from(header.varint()?).map_err(|e| e.to_string())?,
                record: u32::try_from(header.varint()?).map_err(|e| e.to_string())?,
            };
            Ok(Frame {
                layer,
                seq,
                payload: reader.take(payload)?,
            })
        };
        parse().expect("Sorter frames are written by Sorter::add")
    }
}

impl SortEntry {
    /// The order of `SortKey`, on buffered frames.
    fn cmp(&self, other: &Self, frames: &[u8]) -> Ordering {
        let frame = |entry: &Self| Frame::at(&frames[entry.offset as usize..]);
        self.tile_id
            .cmp(&other.tile_id)
            .then_with(|| {
                if self.layer != LAYER_VERBATIM && other.layer != LAYER_VERBATIM {
                    return self.layer.cmp(&other.layer);
                }
                let name = |entry: &Self| {
                    frame(entry)
                        .layer
                        .unwrap_or_else(|| LAYER_NAMES[usize::from(entry.layer)])
                };
                name(self).cmp(name(other))
            })
            .then_with(|| self.sort_key.cmp(&other.sort_key))
            .then_with(|| self.id.cmp(&other.id))
            .then_with(|| frame(self).seq.cmp(&frame(other).seq))
    }

    /// The whole sort key, with the frame's layer name and sequence.
    fn key(&self, frame: &Frame<'_>) -> SortKey {
        SortKey {
            tile_id: self.tile_id,
            layer: frame
                .layer
                .map_or(Layer::Known(self.layer), |name| Layer::Other(name.into())),
            sort_key: self.sort_key,
            id: self.id,
            seq: frame.seq,
        }
    }
}

impl Sorter {
    pub(super) fn add(&mut self, record: EncodedRecord) -> Result<(), String> {
        let bytes = record.frame_bytes();
        if self.frames.len().saturating_add(bytes) > self.budget && !self.entries.is_empty() {
            self.spill()?;
        }
        if self.frames.capacity() == 0 {
            // The buffer fills to the budget before it spills; reserving it
            // whole saves growing (and holding up to twice) as it fills.
            // `spill` reserves budgets grown past this.
            self.frames.reserve(self.budget.min(MAX_RECORD_BYTES));
        }
        let offset = u32::try_from(self.frames.len())
            .map_err(|_| "OMT sort buffer exceeds 4 GiB".to_string())?;
        let header = record.header();
        self.frames.push(header.len as u8);
        self.frames.extend_from_slice(&header.bytes[..header.len]);
        self.frames.extend_from_slice(&record.payload);
        self.entries.push(SortEntry {
            tile_id: record.key.tile_id,
            sort_key: record.key.sort_key,
            id: record.key.id,
            offset,
            layer: match record.key.layer {
                Layer::Known(id) => id,
                Layer::Other(_) => LAYER_VERBATIM,
            },
        });
        Ok(())
    }
    fn spill(&mut self) -> Result<(), String> {
        if self.entries.is_empty() {
            return Ok(());
        }
        let _phase = crate::profiling::Phase::start("omt_spill_sort_write");
        let profiled = crate::stage_profile::enabled().then(std::time::Instant::now);
        // Keys are unique (they end in the insertion sequence).
        let frames = &self.frames;
        self.entries.sort_unstable_by(|a, b| a.cmp(b, frames));
        let path = self.directory.join(format!(
            "run-w{:03}-{:06}.bin",
            self.worker,
            self.runs.len()
        ));
        let mut writer = RunWriter::create(&path)?;
        self.runs.push(path);
        for entry in &self.entries {
            let frame = Frame::at(&frames[entry.offset as usize..]);
            writer.write_parts(&entry.key(&frame), frame.payload)?;
        }
        let spilled = writer.finish()?;
        self.spilled.fetch_add(spilled, AtomicOrdering::Relaxed);
        let run_frames = self.frames.len() as u64;
        self.spilled_frames += run_frames;
        self.spilled_records += self.entries.len() as u64;
        if let Some(started) = profiled {
            crate::stage_profile::add(
                crate::stage_profile::Counter::SpillSortWrite,
                started.elapsed(),
                spilled,
            );
        }
        self.entries.clear();
        self.frames.clear();
        let budget = self.limits.after_spill(
            self.budget,
            run_frames,
            self.spilled_frames,
            self.spilled_records,
        );
        if budget > self.budget {
            self.budget = budget;
            // Free the old buffer before reserving the new one, so the two
            // are never held together.
            self.frames = Vec::new();
            self.frames.reserve_exact(budget);
        }
        Ok(())
    }
    /// Spills the remainder and hands the run files to the caller.
    pub(super) fn finish(mut self) -> Result<Vec<PathBuf>, String> {
        self.spill()?;
        Ok(std::mem::take(&mut self.runs))
    }
}
impl Drop for Sorter {
    fn drop(&mut self) {
        for run in &self.runs {
            let _ = fs::remove_file(run);
        }
    }
}

/// A failed or panicking compile removes the sort directory too, unless its
/// runs are checkpointed for a later compile to resume from.
impl Drop for Store {
    fn drop(&mut self) {
        if self.checkpoints.is_none() {
            let _ = fs::remove_dir_all(&self.directory);
        }
    }
}

impl Store {
    /// Claims `temp` for one compile. A lock file beside the sort directory
    /// refuses a second compile given the same scratch directory, and tells a
    /// sort directory left by a killed compile, which is replaced, from one in
    /// use. The lock file itself stays: removing it would let two compiles
    /// lock different files.
    pub fn new(temp: &Path) -> Result<Self, String> {
        let (directory, lock) = Self::lock(temp)?;
        if directory.exists() {
            fs::remove_dir_all(&directory).map_err(|e| {
                format!(
                    "Failed to remove {} left by an interrupted compile: {e}",
                    directory.display()
                )
            })?;
        }
        fs::create_dir(&directory)
            .map_err(|e| format!("Failed to create {}: {e}", directory.display()))?;
        Ok(Self {
            directory,
            _lock: lock,
            runs: vec![],
            spilled: Arc::new(AtomicU64::new(0)),
            sequence: 0,
            fields: BTreeMap::new(),
            zooms: BTreeMap::new(),
            checkpoints: None,
        })
    }

    /// Takes up the runs an interrupted compile checkpointed in `temp`,
    /// finishing or undoing the merge pass it was in, and checks that every
    /// run is there, whole. The checkpoint stays in use: the merge goes on
    /// recording its passes.
    pub(crate) fn resume(
        temp: &Path,
        sort: &crate::resume::SortCheckpoint,
        checkpoints: crate::resume::Checkpoints,
    ) -> Result<Self, String> {
        let (directory, lock) = Self::lock(temp)?;
        let invalid = |why: String| format!("the checkpoint cannot be resumed: {why}");
        let mut runs: Vec<String> = sort.runs.iter().map(|(name, _)| name.clone()).collect();
        if !sort.pending.is_empty() {
            let mut done = vec![];
            for (inputs, output) in &sort.pending {
                let output_path = directory.join(output);
                if inputs.iter().all(|run| directory.join(run).is_file()) {
                    // Interrupted before its runs were removed: merged again.
                    let _ = fs::remove_file(&output_path);
                } else if output_path.is_file() {
                    for run in inputs {
                        let _ = fs::remove_file(directory.join(run));
                    }
                    done.push((inputs, output.clone()));
                } else {
                    return Err(invalid(format!("{output} is missing")));
                }
            }
            for (inputs, output) in done {
                runs.retain(|run| !inputs.contains(run));
                runs.push(output);
            }
        }
        let sizes = sort.runs.iter().cloned().collect::<BTreeMap<_, _>>();
        for run in &runs {
            let path = directory.join(run);
            let bytes = fs::metadata(&path)
                .map_err(|e| invalid(format!("{}: {e}", path.display())))?
                .len();
            if sizes.get(run).is_some_and(|expected| *expected != bytes) {
                return Err(invalid(format!("{} was changed", path.display())));
            }
        }
        // Anything else is a merge output an interrupted pass left unfinished.
        for entry in fs::read_dir(&directory).map_err(|e| invalid(e.to_string()))? {
            let entry = entry.map_err(|e| invalid(e.to_string()))?;
            if !runs.iter().any(|run| entry.file_name() == run.as_str()) {
                let _ = fs::remove_file(entry.path());
            }
        }
        let store = Self {
            runs: runs.iter().map(|run| directory.join(run)).collect(),
            directory,
            _lock: lock,
            spilled: Arc::new(AtomicU64::new(sort.spilled)),
            sequence: sort.sequence,
            fields: sort.fields.clone(),
            zooms: sort.zooms.clone(),
            checkpoints: Some(checkpoints),
        };
        if !sort.pending.is_empty() {
            store.commit_sort(&[])?;
        }
        Ok(store)
    }

    /// Syncs the runs to disk and returns what a checkpoint of them records.
    /// From then on the merge records each of its passes in `checkpoints`,
    /// and the runs outlive a failed compile.
    pub(crate) fn checkpoint(
        &mut self,
        checkpoints: crate::resume::Checkpoints,
    ) -> Result<crate::resume::SortCheckpoint, String> {
        for run in &self.runs {
            crate::resume::sync_file(run)?;
        }
        self.checkpoints = Some(checkpoints);
        self.sort_checkpoint(&[])
    }

    fn sort_checkpoint(
        &self,
        pending: &[(Vec<PathBuf>, PathBuf)],
    ) -> Result<crate::resume::SortCheckpoint, String> {
        let name = |path: &Path| {
            path.file_name()
                .unwrap_or_default()
                .to_string_lossy()
                .into_owned()
        };
        Ok(crate::resume::SortCheckpoint {
            runs: self
                .runs
                .iter()
                .map(|run| Ok((name(run), run_sizes(std::slice::from_ref(run))?[0])))
                .collect::<Result<_, String>>()?,
            pending: pending
                .iter()
                .map(|(inputs, output)| {
                    (inputs.iter().map(|run| name(run)).collect(), name(output))
                })
                .collect(),
            sequence: self.sequence,
            spilled: self.spilled.load(AtomicOrdering::Relaxed),
            fields: self.fields.clone(),
            zooms: self.zooms.clone(),
        })
    }

    /// Records the runs, and the merge pass about to start, when checkpointed.
    fn commit_sort(&self, pending: &[(Vec<PathBuf>, PathBuf)]) -> Result<(), String> {
        match &self.checkpoints {
            Some(checkpoints) => checkpoints.commit_sort(self.sort_checkpoint(pending)?),
            None => Ok(()),
        }
    }

    /// Starts a pass of the intermediate merge as `finish_cached` does, and
    /// stops as a kill would: after merging `groups_done` of its groups, and
    /// with the next group's output half written.
    #[cfg(test)]
    pub(super) fn interrupt_merge_pass(&mut self, groups_done: usize) {
        let plan = merge_plan(&run_sizes(&self.runs).unwrap());
        let groups = plan
            .iter()
            .map(|group| group.iter().map(|&run| self.runs[run].clone()).collect())
            .collect::<Vec<Vec<_>>>();
        let outputs = groups.iter().map(|_| self.next_path()).collect::<Vec<_>>();
        assert!(groups.len() > groups_done, "{} groups", groups.len());
        let pending = groups.iter().cloned().zip(outputs.iter().cloned());
        self.commit_sort(&pending.collect::<Vec<_>>()).unwrap();
        let control = Control::detached();
        merge_groups(
            &groups[..groups_done],
            &outputs[..groups_done],
            1,
            &control,
            true,
        )
        .unwrap();
        fs::write(&outputs[groups_done], b"half a run").unwrap();
    }

    /// Locks the sort directory of `temp` for this compile.
    fn lock(temp: &Path) -> Result<(PathBuf, File), String> {
        let directory = temp.join("omt-tile-sort");
        let lock_path = temp.join("omt-tile-sort.lock");
        let lock = File::options()
            .create(true)
            .truncate(false)
            .write(true)
            .open(&lock_path)
            .map_err(|e| format!("Failed to create {}: {e}", lock_path.display()))?;
        match lock.try_lock() {
            Ok(()) => {}
            Err(fs::TryLockError::WouldBlock) => {
                return Err(format!("{} is in use by another compile", temp.display()));
            }
            Err(fs::TryLockError::Error(e)) => {
                return Err(format!("Failed to lock {}: {e}", lock_path.display()));
            }
        }
        Ok((directory, lock))
    }
    pub(super) fn sorter(&self, worker: usize, limits: &Arc<SortBudget>) -> Sorter {
        Sorter {
            directory: self.directory.clone(),
            worker,
            budget: limits.floor,
            limits: limits.clone(),
            frames: vec![],
            entries: vec![],
            runs: vec![],
            spilled_frames: 0,
            spilled_records: 0,
            spilled: self.spilled.clone(),
        }
    }
    pub(super) fn add_runs(&mut self, runs: impl IntoIterator<Item = PathBuf>) {
        self.runs.extend(runs);
    }
    fn next_path(&mut self) -> PathBuf {
        let path = self
            .directory
            .join(format!("merge-{:06}.bin", self.sequence));
        self.sequence += 1;
        path
    }
    /// Writes the archive into `archive`, an empty file or partial archive
    /// the caller owns, and returns the sizes of the tiles written.
    #[cfg(test)]
    pub fn finish(
        self,
        archive: &dyn ArchiveTarget,
        header: &ArchiveHeader,
        threads: usize,
        control: &Control,
    ) -> Result<TileStats, String> {
        self.finish_cached(archive, header, threads, control, None)
    }
    /// As `finish`, reusing layers from an earlier archive and writing a
    /// cache index as `cache` says.
    pub fn finish_cached(
        mut self,
        archive: &dyn ArchiveTarget,
        header: &ArchiveHeader,
        threads: usize,
        control: &Control,
        cache: Option<&mut MergeCache>,
    ) -> Result<TileStats, String> {
        let (bounds, center, options) = (&header.bounds, header.center, header.options);
        let (minzoom, maxzoom) = (header.min_zoom, header.max_zoom);
        if threads == 0 {
            return Err("OMT encoder threads must be positive".into());
        }
        #[cfg(feature = "benchmark-profiling")]
        eprintln!(
            "{}",
            serde_json::json!({"benchmark_counts":"omt_spill_bytes",
            "runs":self.runs.len(),
            "raw_bytes":self.spilled.load(AtomicOrdering::Relaxed),
            "bytes":self.runs.iter().filter_map(|p| fs::metadata(p).ok()).map(|m| m.len()).sum::<u64>()})
        );
        // The final merge reads every record once, and each intermediate pass
        // the records of the runs it merges. Progress counts records as
        // frames, before compression; a run's share of them is taken to be
        // its share of the run files' bytes.
        let run_bytes = self.spilled.load(AtomicOrdering::Relaxed);
        let mut sizes = run_sizes(&self.runs)?;
        let file_bytes = sizes.iter().sum::<u64>();
        let mut intermediate = 0u64;
        while sizes.len() > MERGE_FAN_IN {
            let plan = merge_plan(&sizes);
            let merged = plan.iter().map(|group| group_bytes(&sizes, group));
            let merged = merged.collect::<Vec<_>>();
            intermediate += merged.iter().sum::<u64>();
            sizes = unmerged(sizes, &plan);
            sizes.extend(merged);
        }
        let intermediate = (u128::from(run_bytes) * u128::from(intermediate)
            / u128::from(file_bytes.max(1))) as u64;
        control.set_total(run_bytes.saturating_add(intermediate));
        let merge_phase = crate::profiling::Phase::start("omt_intermediate_merge");
        let merge_stage = crate::stage_profile::Stage::start("intermediate_merge");
        while self.runs.len() > MERGE_FAN_IN {
            let plan = merge_plan(&run_sizes(&self.runs)?);
            let groups = plan
                .iter()
                .map(|group| group.iter().map(|&run| self.runs[run].clone()).collect())
                .collect::<Vec<Vec<_>>>();
            let outputs = groups.iter().map(|_| self.next_path()).collect::<Vec<_>>();
            let pending = groups.iter().cloned().zip(outputs.iter().cloned());
            self.commit_sort(&pending.collect::<Vec<_>>())?;
            let durable = self.checkpoints.is_some();
            // Keys are unique, so any grouping merges to the same record order.
            if let Err(error) = merge_groups(&groups, &outputs, threads, control, durable) {
                // A checkpointed pass is finished or undone on resuming.
                if !durable {
                    for path in &outputs {
                        let _ = fs::remove_file(path);
                    }
                }
                return Err(error);
            }
            // `merge_groups` removed the merged runs.
            self.runs = unmerged(std::mem::take(&mut self.runs), &plan);
            self.runs.extend(outputs);
            self.commit_sort(&[])?;
        }
        drop(merge_phase);
        drop(merge_stage);
        let metadata = metadata(
            header.layers,
            (minzoom, maxzoom),
            &self.fields,
            &self.zooms,
            options,
            header.omitted_sources,
            header.osm_data,
            header.data_layers,
        );
        let mut archive = archive.open_writer(&ArchiveSpec {
            bounds,
            center,
            metadata_json: &metadata,
            min_zoom: minzoom,
            max_zoom: maxzoom,
            tile_format: options.tile_format,
        })?;
        let mut writing = crate::profiling::Accumulator::new("omt_archive_write");
        let mut stats = TileStatsBuilder::new(TILE_WARNING_BYTES);
        let final_merge = crate::profiling::Phase::start("omt_final_merge_encode_write");
        let final_stage = crate::stage_profile::Stage::start("final_merge");
        let (mut reuse, mut writer, counters) = match cache {
            Some(cache) => (
                cache.reuse.as_mut(),
                cache.writer.as_mut(),
                Some(&cache.counters),
            ),
            None => (None, None, None),
        };
        let hashing = reuse.is_some() || writer.is_some();
        let old_archive = reuse.as_ref().map(|reuse| reuse.archive.clone());
        let encoded = encode_ordered(
            threads,
            threads.saturating_mul(TILE_WINDOW_PER_THREAD),
            CHUNK_BYTES,
            || {
                (
                    EncodeTimings::new(),
                    crate::profiling::Accumulator::new("omt_encode_tile_inclusive"),
                    None::<File>,
                )
            },
            |(timings, inclusive, old_file), job: TileJob| {
                let id = job.id;
                let reuse = old_archive.as_deref().map(|path| (path, old_file));
                crate::stage_profile::time(crate::stage_profile::Counter::EncodeTile, || {
                    inclusive.measure(|| {
                        encode_job(
                            job,
                            hashing,
                            reuse,
                            counters,
                            timings,
                            options.gzip_level,
                            options.tile_format,
                            control,
                        )
                    })
                })
                .map(|(bytes, entry)| (id, bytes, entry))
            },
            |(id, tile, entry): (u64, Option<EncodedTile>, Option<TileEntry>)| {
                if let (Some(writer), Some(entry)) = (writer.as_deref_mut(), &entry) {
                    writer.add(entry)?;
                }
                if let Some(tile) = tile {
                    crate::stage_profile::time(
                        crate::stage_profile::Counter::ArchiveWrite,
                        || writing.measure(|| archive.write_tile(id, &tile.bytes)),
                    )?;
                    let layers = tile
                        .layers
                        .iter()
                        .map(|(name, bytes)| (name.as_str(), *bytes))
                        .collect::<Vec<_>>();
                    stats.add(id, tile.bytes.len() as u64, &layers);
                }
                Ok(())
            },
            |stream| {
                let mut tile_id = None;
                let mut tile = vec![];
                let mut tile_bytes = 0usize;
                merge_prefetched(&self.runs, threads, control, |encoded| {
                    if tile_id.is_some_and(|id| id != encoded.key.tile_id) {
                        let id = tile_id.unwrap();
                        let job = TileJob {
                            id,
                            records: std::mem::take(&mut tile),
                            old: match reuse.as_deref_mut() {
                                Some(reuse) => reuse.take(id)?,
                                None => None,
                            },
                        };
                        stream.send(job, tile_bytes)?;
                        tile_bytes = 0;
                    }
                    tile_id = Some(encoded.key.tile_id);
                    tile_bytes = tile_bytes.saturating_add(encoded.payload.len());
                    if tile_bytes > MAX_TILE_BYTES {
                        return Err("OMT tile exceeds bounded 256MiB record budget".into());
                    }
                    tile.push(encoded);
                    Ok(())
                })?;
                if let Some(id) = tile_id {
                    let old = match reuse {
                        Some(reuse) => reuse.take(id)?,
                        None => None,
                    };
                    stream.send(
                        TileJob {
                            id,
                            records: tile,
                            old,
                        },
                        tile_bytes,
                    )?;
                }
                Ok(())
            },
        );
        drop(final_merge);
        drop(final_stage);
        if let Err(error) = encoded {
            if self.checkpoints.is_none() {
                for path in &self.runs {
                    let _ = fs::remove_file(path);
                }
            }
            return Err(error);
        }
        let finalizing = crate::profiling::Phase::start("omt_archive_finalize");
        let finalize_stage = crate::stage_profile::Stage::start("finalize_archive");
        archive.finalize()?;
        drop(finalizing);
        drop(finalize_stage);
        for path in std::mem::take(&mut self.runs) {
            fs::remove_file(path).map_err(|e| e.to_string())?;
        }
        fs::remove_dir(&self.directory).map_err(|e| e.to_string())?;
        Ok(stats.finish())
    }
}

/// The sizes of the run files at `runs`.
fn run_sizes(runs: &[PathBuf]) -> Result<Vec<u64>, String> {
    runs.iter()
        .map(|run| {
            fs::metadata(run)
                .map(|metadata| metadata.len())
                .map_err(|e| format!("Failed to read {}: {e}", run.display()))
        })
        .collect()
}

/// The runs, of these `sizes`, that the next intermediate pass merges, as
/// groups of indexes: the fewest and smallest runs whose merging leaves at
/// most `MERGE_FAN_IN` runs for the final merge, or all of them, in groups
/// of at most `MERGE_FAN_IN`, when one pass can't get that far. Growing sort
/// buffers (`SortBudget`) leave many small early runs and few large ones, so
/// a pass usually rewrites a small part of the records rather than all.
fn merge_plan(sizes: &[u64]) -> Vec<Vec<usize>> {
    let (runs, fan_in) = (sizes.len(), MERGE_FAN_IN);
    if runs <= fan_in {
        return vec![];
    }
    // Each group of k runs turns into one, so g groups taking
    // runs - fan_in + g runs between them leave exactly fan_in.
    let groups = (runs - fan_in).div_ceil(fan_in - 1);
    let (groups, merged) = if groups <= fan_in {
        (groups, runs - fan_in + groups)
    } else {
        (runs.div_ceil(fan_in), runs)
    };
    let mut order = (0..runs).collect::<Vec<_>>();
    order.sort_by_key(|&run| (sizes[run], run));
    (0..groups)
        .map(|group| order[group * merged / groups..(group + 1) * merged / groups].to_vec())
        .collect()
}

fn group_bytes(sizes: &[u64], group: &[usize]) -> u64 {
    group.iter().map(|&run| sizes[run]).sum()
}

/// The items of `items` that no group of `plan` takes, in their order.
fn unmerged<T>(items: Vec<T>, plan: &[Vec<usize>]) -> Vec<T> {
    let mut merged = vec![false; items.len()];
    for &index in plan.iter().flatten() {
        merged[index] = true;
    }
    items
        .into_iter()
        .zip(merged)
        .filter_map(|(item, merged)| (!merged).then_some(item))
        .collect()
}

/// Merges each group of runs into the matching path in `outputs`, at most
/// `threads` groups at a time, and removes a group's runs once it is merged,
/// so a pass needs scratch space for one group's records twice, not all.
fn merge_groups(
    groups: &[Vec<PathBuf>],
    outputs: &[PathBuf],
    threads: usize,
    control: &Control,
    durable: bool,
) -> Result<(), String> {
    let next = AtomicUsize::new(0);
    let merge_group = |group: usize| -> Result<(), String> {
        let batch = &groups[group];
        let mut writer = RunWriter::create(&outputs[group])?;
        merge_controlled(batch, control, |record| writer.write(&record))?;
        writer.finish()?;
        // A checkpointed merge's output is on disk before its runs go.
        if durable {
            crate::resume::sync_file(&outputs[group])?;
        }
        for run in batch {
            fs::remove_file(run).map_err(|e| format!("Failed to remove {}: {e}", run.display()))?;
        }
        Ok(())
    };
    let results = std::thread::scope(|scope| {
        let workers = (0..threads.min(outputs.len()))
            .map(|_| {
                scope.spawn(|| {
                    let mut done = vec![];
                    loop {
                        let group = next.fetch_add(1, AtomicOrdering::Relaxed);
                        if group >= outputs.len() {
                            return done;
                        }
                        let result = merge_group(group);
                        let failed = result.is_err();
                        done.push((group, result));
                        if failed {
                            next.store(outputs.len(), AtomicOrdering::Relaxed);
                            return done;
                        }
                    }
                })
            })
            .collect::<Vec<_>>();
        workers
            .into_iter()
            .map(|worker| {
                worker
                    .join()
                    .unwrap_or_else(|panic| std::panic::resume_unwind(panic))
            })
            .collect::<Vec<_>>()
    });
    results
        .into_iter()
        .flatten()
        .filter_map(|(group, result)| result.err().map(|error| (group, error)))
        .min()
        .map_or(Ok(()), |(_, error)| Err(error))
}

// Tiles in flight (dispatched but not yet written) are capped at this many per
// encoder thread, so a slow tile blocks writing but not dispatch of later ones.
const TILE_WINDOW_PER_THREAD: usize = 8;

// The byte budget counts serialized input bytes, not heap/RSS. A single
// oversized tile still runs, alone: the merge loop holds at most one more tile
// while it assembles the next job.
struct TileJob {
    id: u64,
    records: Vec<EncodedRecord>,
    /// The tile in the archive being reused, if any.
    old: Option<OldTile>,
}
impl TileJob {
    #[cfg(test)]
    fn decode(self) -> Result<Vec<Record>, String> {
        self.records
            .into_iter()
            .map(|encoded| {
                if encoded.key.tile_id != self.id {
                    return Err("OMT tile job contains a different tile".into());
                }
                encoded.decode()
            })
            .collect()
    }
}

/// A finished job: its dispatch index, its input bytes and its result.
type Done<R> = (usize, usize, Result<R, String>);

/// Producer side of `encode_ordered`. Results reach the writer in dispatch
/// order; those that finish early wait in `pending`. The first failure, in
/// dispatch order, raises `failed` and is returned from the call that saw it.
struct Stream<'a, J, R, W> {
    jobs: crossbeam_channel::Sender<(usize, usize, J)>,
    done: crossbeam_channel::Receiver<Done<R>>,
    failed: &'a AtomicBool,
    write: W,
    pending: BTreeMap<usize, Result<R, String>>,
    sent: usize,
    written: usize,
    queued_bytes: usize,
    window: usize,
    byte_budget: usize,
}
impl<J, R, W: FnMut(R) -> Result<(), String>> Stream<'_, J, R, W> {
    /// Queues a job of `bytes` input bytes, first writing finished tiles while
    /// the window or byte budget is full. A job larger than the budget is
    /// queued only when nothing else is.
    fn send(&mut self, job: J, bytes: usize) -> Result<(), String> {
        while self.sent - self.written >= self.window
            || (self.queued_bytes > 0 && self.queued_bytes.saturating_add(bytes) > self.byte_budget)
        {
            self.receive()?;
        }
        self.jobs
            .send((self.sent, bytes, job))
            .map_err(|_| self.stop("OMT tile encoders stopped unexpectedly".into()))?;
        self.sent += 1;
        self.queued_bytes = self.queued_bytes.saturating_add(bytes);
        while let Ok(done) = self.done.try_recv() {
            self.accept(done)?;
        }
        Ok(())
    }
    /// Writes every remaining result.
    fn finish(&mut self) -> Result<(), String> {
        while self.written < self.sent {
            self.receive()?;
        }
        Ok(())
    }
    fn receive(&mut self) -> Result<(), String> {
        let done = self
            .done
            .recv()
            .map_err(|_| self.stop("OMT tile encoders stopped unexpectedly".into()))?;
        self.accept(done)
    }
    fn accept(&mut self, (index, bytes, result): Done<R>) -> Result<(), String> {
        self.queued_bytes -= bytes;
        self.pending.insert(index, result);
        while let Some(result) = self.pending.remove(&self.written) {
            if let Err(error) = result.and_then(&mut self.write) {
                return Err(self.stop(error));
            }
            self.written += 1;
        }
        Ok(())
    }
    fn stop(&self, error: String) -> String {
        self.failed.store(true, AtomicOrdering::Relaxed);
        error
    }
}

/// Runs `produce` on this thread to feed jobs to `threads` encoder threads and
/// passes each result to `write` strictly in the order the jobs were sent. At
/// most `window` jobs are in flight and, unless one job alone exceeds it,
/// `byte_budget` input bytes are queued. Each encoder owns the state `init`
/// builds. `produce` must return any error `Stream::send` gives it. The first
/// error in job order wins, with `produce`'s own error after all queued jobs.
/// A panicking encoder counts as a failed job, so nothing waits forever.
fn encode_ordered<J: Send, R: Send, S, W: FnMut(R) -> Result<(), String>>(
    threads: usize,
    window: usize,
    byte_budget: usize,
    init: impl Fn() -> S + Sync,
    encode: impl Fn(&mut S, J) -> Result<R, String> + Sync,
    write: W,
    produce: impl FnOnce(&mut Stream<'_, J, R, W>) -> Result<(), String>,
) -> Result<(), String> {
    let failed = AtomicBool::new(false);
    std::thread::scope(|scope| {
        let (job_sender, job_receiver) = crossbeam_channel::unbounded();
        let (done_sender, done_receiver) = crossbeam_channel::unbounded();
        for worker in 0..threads {
            let (jobs, done) = (job_receiver.clone(), done_sender.clone());
            let (init, encode, failed) = (&init, &encode, &failed);
            std::thread::Builder::new()
                .name(format!("omt-encode-{worker}"))
                .spawn_scoped(scope, move || {
                    let mut state = init();
                    for (index, bytes, job) in jobs {
                        if failed.load(AtomicOrdering::Relaxed) {
                            break;
                        }
                        let result = catch_unwind(AssertUnwindSafe(|| encode(&mut state, job)))
                            .unwrap_or_else(|payload| {
                                let message = crate::panic_message(&*payload);
                                Err(format!("OMT tile encoder panicked: {message}"))
                            });
                        if done.send((index, bytes, result)).is_err() {
                            break;
                        }
                    }
                })
                .map_err(|e| format!("Start OMT tile encoder: {e}"))?;
        }
        drop((job_receiver, done_sender));
        let mut stream = Stream {
            jobs: job_sender,
            done: done_receiver,
            failed: &failed,
            write,
            pending: BTreeMap::new(),
            sent: 0,
            written: 0,
            queued_bytes: 0,
            window: window.max(1),
            byte_budget,
        };
        let produced = produce(&mut stream);
        if failed.load(AtomicOrdering::Relaxed) {
            return produced;
        }
        // A producer error follows every job it queued before failing.
        stream.finish().and(produced)
    })
}

struct Head {
    record: EncodedRecord,
    reader: usize,
}
impl PartialEq for Head {
    fn eq(&self, other: &Self) -> bool {
        self.record.key.cmp(&other.record.key) == Ordering::Equal && self.reader == other.reader
    }
}
impl Eq for Head {}
impl PartialOrd for Head {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for Head {
    fn cmp(&self, other: &Self) -> Ordering {
        self.record
            .key
            .cmp(&other.record.key)
            .then(self.reader.cmp(&other.reader))
    }
}

/// A run's next decoded block and its records' size as frames: empty at the
/// end of the run.
type Block = Result<(Vec<EncodedRecord>, u64), String>;

/// Merges sorted runs as `merge_controlled` does, record for record, while
/// `decoders` threads read and decode the runs' blocks ahead of the merge,
/// so that this thread only orders records. Each run has `BLOCKS_AHEAD`
/// blocks requested at a time: the merge asks for another as it starts on
/// one, so the blocks held decoded stay bounded by the runs, not their size.
fn merge_prefetched(
    paths: &[PathBuf],
    decoders: usize,
    control: &Control,
    mut emit: impl FnMut(EncodedRecord) -> Result<(), String>,
) -> Result<(), String> {
    const REPORT_BYTES: u64 = 1 << 20;
    let readers = paths
        .iter()
        .map(|p| RunReader::open(p).map(Mutex::new))
        .collect::<Result<Vec<_>, _>>()?;
    let (block_senders, block_receivers): (Vec<_>, Vec<_>) = (0..paths.len())
        .map(|_| crossbeam_channel::unbounded::<Block>())
        .unzip();
    let (requests, requested) = crossbeam_channel::unbounded::<usize>();
    let stop = AtomicBool::new(false);
    std::thread::scope(|scope| {
        // Dropped however this closure returns, which ends the decoders
        // before the scope waits for them.
        let requests = requests;
        let _stop = StopOnDrop(&stop);
        for worker in 0..decoders.max(1).min(paths.len()) {
            let (requested, readers, block_senders, stop) =
                (requested.clone(), &readers, &block_senders, &stop);
            std::thread::Builder::new()
                .name(format!("omt-run-decode-{worker}"))
                .spawn_scoped(scope, move || {
                    for run in requested {
                        if stop.load(AtomicOrdering::Relaxed) {
                            return;
                        }
                        let mut reader = readers[run].lock().unwrap_or_else(|e| e.into_inner());
                        let block = catch_unwind(AssertUnwindSafe(|| {
                            let records = reader.next_block_records()?;
                            let bytes = records.iter().map(|r| r.frame_bytes() as u64).sum();
                            Ok((records, bytes))
                        }))
                        .unwrap_or_else(|payload| {
                            let message = crate::panic_message(&*payload);
                            Err(format!("OMT run decoder panicked: {message}"))
                        });
                        // Sent while the run is locked, so its blocks arrive in order.
                        if block_senders[run].send(block).is_err() {
                            return;
                        }
                    }
                })
                .map_err(|e| format!("Start OMT run decoder: {e}"))?;
        }
        drop(requested);
        for run in 0..paths.len() {
            for _ in 0..BLOCKS_AHEAD {
                requests
                    .send(run)
                    .map_err(|_| "OMT run decoders stopped unexpectedly")?;
            }
        }
        let mut blocks = (0..paths.len())
            .map(|_| Vec::new().into_iter())
            .collect::<Vec<_>>();
        let mut ended = vec![false; paths.len()];
        // Progress counts a block's bytes as the merge starts on it.
        let mut merged = 0;
        let mut next = |run: usize| -> Result<Option<EncodedRecord>, String> {
            loop {
                if let Some(record) = blocks[run].next() {
                    return Ok(Some(record));
                }
                if ended[run] {
                    return Ok(None);
                }
                let (block, bytes) = block_receivers[run]
                    .recv()
                    .map_err(|_| "OMT run decoders stopped unexpectedly")??;
                if block.is_empty() {
                    ended[run] = true;
                    return Ok(None);
                }
                requests
                    .send(run)
                    .map_err(|_| "OMT run decoders stopped unexpectedly")?;
                merged += bytes;
                if merged >= REPORT_BYTES {
                    control.add_done(merged);
                    merged = 0;
                    control.check()?;
                }
                blocks[run] = block.into_iter();
            }
        };
        let mut heap = BinaryHeap::new();
        for reader in 0..paths.len() {
            if let Some(record) = next(reader)? {
                heap.push(Reverse(Head { record, reader }));
            }
        }
        // The head's successor from the same run replaces it in place: one
        // sift instead of a pop and a push.
        while let Some(mut top) = heap.peek_mut() {
            let reader = top.0.reader;
            let record = match next(reader) {
                Ok(Some(record)) => std::mem::replace(&mut top.0.record, record),
                Ok(None) => PeekMut::pop(top).0.record,
                // The head comes first in record order, and so does its error.
                Err(error) => {
                    emit(PeekMut::pop(top).0.record)?;
                    return Err(error);
                }
            };
            emit(record)?;
        }
        drop(next);
        control.add_done(merged);
        Ok(())
    })
}

/// Raises its flag when dropped, so that threads still working stop early.
struct StopOnDrop<'a>(&'a AtomicBool);
impl Drop for StopOnDrop<'_> {
    fn drop(&mut self) {
        self.0.store(true, AtomicOrdering::Relaxed);
    }
}

#[cfg(test)]
fn merge(
    paths: &[PathBuf],
    emit: impl FnMut(EncodedRecord) -> Result<(), String>,
) -> Result<(), String> {
    merge_controlled(paths, &Control::detached(), emit)
}

/// Merges sorted runs, adding the bytes merged to `control`'s progress and
/// stopping on a cancel within a MiB of records.
fn merge_controlled(
    paths: &[PathBuf],
    control: &Control,
    mut emit: impl FnMut(EncodedRecord) -> Result<(), String>,
) -> Result<(), String> {
    const REPORT_BYTES: u64 = 1 << 20;
    let mut readers = paths
        .iter()
        .map(|p| RunReader::open(p))
        .collect::<Result<Vec<_>, _>>()?;
    let mut heap = BinaryHeap::new();
    for (index, reader) in readers.iter_mut().enumerate() {
        if let Some(record) = reader.next()? {
            heap.push(Reverse(Head {
                record,
                reader: index,
            }));
        }
    }
    let mut merged = 0;
    while let Some(Reverse(head)) = heap.pop() {
        merged += head.record.frame_bytes() as u64;
        if merged >= REPORT_BYTES {
            control.add_done(merged);
            merged = 0;
            control.check()?;
        }
        emit(head.record)?;
        if let Some(record) = readers[head.reader].next()? {
            heap.push(Reverse(Head {
                record,
                reader: head.reader,
            }));
        }
    }
    control.add_done(merged);
    Ok(())
}

/// The metadata a header gives, parsed, with no layer fields or zooms.
#[cfg(test)]
pub(super) fn metadata_for_test(header: &ArchiveHeader) -> serde_json::Value {
    serde_json::from_str(&metadata(
        header.layers,
        (header.min_zoom, header.max_zoom),
        &BTreeMap::new(),
        &BTreeMap::new(),
        header.options,
        header.omitted_sources,
        header.osm_data,
        header.data_layers,
    ))
    .unwrap()
}

/// The metadata's `generator`: this crate and its version.
pub(crate) const GENERATOR: &str = concat!("osm-tile-compiler ", env!("CARGO_PKG_VERSION"));

#[allow(clippy::too_many_arguments)]
fn metadata(
    layers: &[&str],
    (min, max): (u8, u8),
    fields: &BTreeMap<String, BTreeMap<String, String>>,
    zooms: &BTreeMap<String, (u8, u8)>,
    options: &ArchiveOptions,
    omitted_sources: &[&str],
    osm_data: &super::OsmDataInfo,
    data_layers: &[serde_json::Value],
) -> String {
    let layers_written = layers;
    let layers = layers
        .iter()
        .map(|id| {
            let (lo, hi) = zooms.get(*id).copied().unwrap_or((min, max));
            serde_json::json!({
                "id": id,
                "minzoom": lo,
                "maxzoom": hi,
                "fields": fields.get(*id).cloned().unwrap_or_default(),
            })
        })
        .collect::<Vec<_>>();
    let mut metadata = serde_json::json!({
        "name": options.name,
        "version": super::OMT_SCHEMA_VERSION,
        "type": "baselayer",
        "format": options.tile_format.metadata_format(),
        "minzoom": min,
        "maxzoom": max,
        "description": options.description,
        "attribution": options.attribution,
        "planetiler_openmaptiles_profile_commit": super::PROFILE_REVISION,
        "generator": GENERATOR,
        "vector_layers": layers,
    });
    // Each is written only when the extract's header gives it.
    if let Some(time) = osm_data.timestamp_rfc3339() {
        metadata["osm_data_timestamp"] = serde_json::json!(time);
    }
    if let Some(sequence) = osm_data.replication_sequence_number {
        metadata["osm_replication_sequence"] = serde_json::json!(sequence);
    }
    if let Some(url) = &osm_data.replication_base_url {
        metadata["osm_replication_url"] = serde_json::json!(url);
    }
    // Only an archive built without some supplemental datasets says so, so
    // a full build's metadata is as before.
    if !omitted_sources.is_empty() {
        metadata["omitted_sources"] = serde_json::json!(omitted_sources);
    }
    // And only one with data layers says where they came from.
    if !data_layers.is_empty() {
        metadata["data_layers"] = serde_json::json!(data_layers);
    }
    // Likewise only an archive with the opt-in layer names its schema.
    if layers_written.contains(&super::sar::LAYER) {
        metadata["sar_schema"] = serde_json::json!(super::sar::SCHEMA_VERSION);
    }
    metadata.to_string()
}

struct EncodeTimings {
    decode: crate::profiling::Accumulator,
    density: crate::profiling::Accumulator,
    geometry: crate::profiling::Accumulator,
    dictionaries: crate::profiling::Accumulator,
    protobuf_gzip: crate::profiling::Accumulator,
    hash: crate::profiling::Accumulator,
    reuse: crate::profiling::Accumulator,
    mlt: crate::profiling::Accumulator,
}
impl EncodeTimings {
    fn new() -> Self {
        Self {
            decode: crate::profiling::Accumulator::new("omt_tile_record_decode"),
            density: crate::profiling::Accumulator::new("omt_point_density"),
            geometry: crate::profiling::Accumulator::new("omt_merge_geometries"),
            dictionaries: crate::profiling::Accumulator::new("omt_mvt_geometry_dictionaries"),
            protobuf_gzip: crate::profiling::Accumulator::new("omt_protobuf_gzip"),
            hash: crate::profiling::Accumulator::new("omt_merge_cache_hash"),
            reuse: crate::profiling::Accumulator::new("omt_merge_cache_read"),
            mlt: crate::profiling::Accumulator::new("omt_mlt_transcode"),
        }
    }
}
/// An encoded tile and the uncompressed size of each of its layers.
struct EncodedTile {
    bytes: Vec<u8>,
    layers: Vec<(String, u64)>,
}

/// A layer's records merged and encoded as a tile's layer field, or `None`
/// when no feature is left.
fn encode_layer_records(
    name: &str,
    mut records: Vec<Record>,
    timings: &mut EncodeTimings,
    tile_format: TileFormat,
    control: &Control,
) -> Result<Option<Vec<u8>>, String> {
    control.check()?;
    records.sort_by(order);
    records = timings.density.measure(|| point_density(records));
    records = timings
        .geometry
        .measure(|| merge_geometries_controlled(records, control))?;
    let layer = timings
        .dictionaries
        .measure(|| encode_layer(name, &records))
        .map_err(|e| format!("Failed to encode layer {name}: {e}"))?;
    if layer.num_features() == 0 {
        return Ok(None);
    }
    let mvt = timings.protobuf_gzip.measure(|| layer.encode());
    match tile_format {
        TileFormat::Mvt => Ok(Some(mvt)),
        // The MVT layer is transcoded, so that both formats hold the same
        // features down to the coordinate.
        TileFormat::Mlt => {
            let mlt = timings
                .mlt
                .measure(|| crate::mlt::from_mvt(&mvt))
                .map_err(|e| format!("Failed to encode layer {name} as MLT: {e}"))?;
            Ok((!mlt.is_empty()).then_some(mlt))
        }
    }
}

/// Gzips a tile's layers; `None` when it has none.
fn finish_tile(
    tile: Vec<u8>,
    sizes: Vec<(String, u64)>,
    timings: &mut EncodeTimings,
    gzip_level: u32,
) -> Result<Option<EncodedTile>, String> {
    if sizes.is_empty() {
        return Ok(None);
    }
    let bytes = timings
        .protobuf_gzip
        .measure(|| gzip(&tile, gzip_level))
        .map_err(|e| format!("Failed to gzip a tile: {e}"))?;
    Ok(Some(EncodedTile {
        bytes,
        layers: sizes,
    }))
}

/// Encodes a tile job. With `hashing`, also returns the tile's cache entry;
/// with `reuse` (the old archive and this worker's handle on it), layers
/// whose records match the old tile's are taken from it.
#[allow(clippy::too_many_arguments)]
fn encode_job(
    job: TileJob,
    hashing: bool,
    reuse: Option<(&Path, &mut Option<File>)>,
    counters: Option<&merge_cache::Counters>,
    timings: &mut EncodeTimings,
    gzip_level: u32,
    tile_format: TileFormat,
    control: &Control,
) -> Result<(Option<EncodedTile>, Option<TileEntry>), String> {
    let id = job.id;
    // Records arrive sorted by layer, so each layer is one run of them.
    let mut groups: Vec<(Layer, Vec<EncodedRecord>)> = Vec::new();
    for record in job.records {
        if record.key.tile_id != id {
            return Err("OMT tile job contains a different tile".into());
        }
        match groups.last_mut() {
            Some((layer, records)) if *layer == record.key.layer => records.push(record),
            _ => groups.push((record.key.layer.clone(), vec![record])),
        }
    }
    // Layers are encoded in name order, as a map by name would hold them.
    groups.sort_by(|a, b| a.0.name().cmp(b.0.name()));
    if groups.windows(2).any(|w| w[0].0 == w[1].0) {
        return Err("OMT tile job holds a layer twice".into());
    }
    let keys = if hashing {
        timings.hash.measure(|| {
            groups
                .iter()
                .map(|(layer, records)| merge_cache::layer_key(id, layer.name(), records))
                .collect::<Vec<_>>()
        })
    } else {
        Vec::new()
    };
    let old = job.old.as_ref().filter(|_| hashing);
    let count = |counter: fn(&merge_cache::Counters) -> &AtomicU64, n: usize| {
        if let Some(counters) = counters {
            merge_cache::Counters::add(counter(counters), n as u64);
        }
    };
    let (path, mut file) = match reuse {
        Some((path, file)) => (Some(path), Some(file)),
        None => (None, None),
    };
    // Every layer the same as before: the old tile, as it is stored.
    if let (Some(old), Some(path), Some(file)) = (old, path, file.as_deref_mut())
        && old.copy
        && old.entry.layers.len() == groups.len()
        && old
            .entry
            .layers
            .iter()
            .zip(groups.iter().zip(&keys))
            .all(|(entry, ((layer, _), key))| *entry.name == *layer.name() && entry.key == *key)
    {
        let tile = if old.entry.stored == 0 {
            None
        } else {
            let bytes = timings
                .reuse
                .measure(|| merge_cache::read_stored(file, path, old))?;
            Some(EncodedTile {
                bytes,
                layers: old
                    .entry
                    .layers
                    .iter()
                    .filter(|layer| layer.size > 0)
                    .map(|layer| (layer.name.to_string(), u64::from(layer.size)))
                    .collect(),
            })
        };
        count(|c| &c.tiles_reused, 1);
        count(|c| &c.layers_reused, groups.len());
        return Ok((tile, Some(old.entry.clone())));
    }
    let mut old_layers: Option<Vec<u8>> = None;
    let mut tile = Vec::new();
    let mut sizes = Vec::new();
    let mut entries = Vec::with_capacity(if hashing { groups.len() } else { 0 });
    for (index, (layer, records)) in groups.into_iter().enumerate() {
        let name = layer.name();
        // The same layer in the old tile, and where its bytes start there.
        let hit = old.and_then(|old| {
            let mut start = 0usize;
            for entry in &old.entry.layers {
                if *entry.name == *name {
                    return (entry.key == keys[index]).then_some((old, start, entry.size));
                }
                start += entry.size as usize;
            }
            None
        });
        let bytes = match (hit, path, file.as_deref_mut()) {
            (Some((old, start, size)), Some(path), Some(file)) => {
                count(|c| &c.layers_reused, 1);
                if size == 0 {
                    None
                } else {
                    if old_layers.is_none() {
                        old_layers = Some(
                            timings
                                .reuse
                                .measure(|| merge_cache::read_layers(file, path, old))?,
                        );
                    }
                    let all = old_layers.as_ref().expect("read above");
                    Some(all[start..start + size as usize].to_vec())
                }
            }
            _ => {
                count(|c| &c.layers_encoded, 1);
                let records = timings.decode.measure(|| {
                    records
                        .into_iter()
                        .map(EncodedRecord::decode)
                        .collect::<Result<Vec<_>, _>>()
                })?;
                encode_layer_records(name, records, timings, tile_format, control)?
            }
        };
        let size = bytes.as_ref().map_or(0, Vec::len);
        if hashing {
            entries.push(LayerEntry {
                name: name.into(),
                key: keys[index],
                size: u32::try_from(size).map_err(|_| "OMT tile layer exceeds 4 GiB")?,
            });
        }
        if let Some(bytes) = bytes {
            tile.extend_from_slice(&bytes);
            sizes.push((name.to_string(), size as u64));
        }
    }
    let tile = finish_tile(tile, sizes, timings, gzip_level)?;
    let entry = hashing.then(|| TileEntry {
        tile_id: id,
        stored: tile.as_ref().map_or(0, |tile| tile.bytes.len() as u32),
        layers: entries,
    });
    Ok((tile, entry))
}

/// The layer's features, in `records` order. Attributes starting with `_` are
/// the compiler's own and are left out.
fn encode_layer(name: &str, records: &[Record]) -> fast_mvt::MvtResult<MvtLayerBuilder> {
    let mut layer = MvtLayerBuilder::new(name)?;
    for record in records {
        let Some(shape) = record.geometry.mvt_shape() else {
            continue;
        };
        let mut feature = layer.feature_of(record.geometry.geometry_type());
        feature.id((record.id != 0).then_some(record.id));
        for (key, value) in &record.attrs {
            if !key.starts_with('_') {
                feature.tag_ref(key, value.mvt())?;
            }
        }
        shape.write(&mut feature)?;
        layer = feature.end();
    }
    Ok(layer)
}

fn gzip(bytes: &[u8], level: u32) -> std::io::Result<Vec<u8>> {
    let mut encoder = flate2::write::GzEncoder::new(
        Vec::with_capacity(bytes.len() / 2),
        flate2::Compression::new(level),
    );
    encoder.write_all(bytes)?;
    encoder.finish()
}

fn point_density(records: Vec<Record>) -> Vec<Record> {
    let mut grids: BTreeMap<(i64, i64), usize> = BTreeMap::new();
    let mut house_best: BTreeMap<String, Record> = BTreeMap::new();
    // Every partition in encounter order: groupingBy's computeIfAbsent can
    // resize the map even on a call that finds an existing key.
    let mut house_partitions = vec![];
    let mut ordinary = vec![];
    for record in records {
        if record.layer == "housenumber"
            && let Some(AttrValue::String(partition)) = record.attrs.get("__housenumber_partition")
        {
            let has_name = matches!(
                record.attrs.get("__housenumber_has_name"),
                Some(AttrValue::Bool(true))
            );
            house_partitions.push(partition.clone());
            let replace = house_best.get(partition).is_none_or(|old| {
                !has_name
                    && matches!(
                        old.attrs.get("__housenumber_has_name"),
                        Some(AttrValue::Bool(true))
                    )
            });
            if replace {
                house_best.insert(partition.clone(), record);
            }
            continue;
        }
        ordinary.push(record);
    }
    ordinary.sort_by(order);
    for index in super::java_hashmap::grouping_by_partition_order(&house_partitions) {
        ordinary.push(
            house_best
                .remove(&house_partitions[index])
                .expect("one selected housenumber per partition"),
        );
    }
    let mut output = vec![];
    for mut record in ordinary {
        if let TileGeometry::Point(points) = &record.geometry
            && let Some(point) = points.first()
        {
            let count = if let Some((x, y)) = record.grid_cell {
                let key = (i64::from(x), i64::from(y));
                let count = grids.entry(key).or_default();
                *count += 1;
                *count
            } else {
                // Planetiler's ungrouped feature.group() is zero; the
                // postprocessor still increments this shared group.
                let count = grids.entry((0, 0)).or_default();
                *count += 1;
                *count
            };
            if record.grid_limit > 0 && record.grid_cell.is_some() && count > record.grid_limit {
                continue;
            }
            if (record.rank || matches!(&*record.layer, "place" | "mountain_peak"))
                && !record.attrs.contains_key("rank")
            {
                record.attrs.insert(
                    "rank".into(),
                    AttrValue::Int(count as i64 + if record.layer == "place" { 10 } else { 0 }),
                );
            }
            if record.layer == "mountain_peak" && !(-1024..=5120).contains(&point.x) {
                continue;
            }
            if record.layer == "mountain_peak" && !(-1024..=5120).contains(&point.y) {
                continue;
            }
        }
        output.push(record);
    }
    output
}

#[cfg(test)]
fn merge_geometries(records: Vec<Record>) -> Result<Vec<Record>, String> {
    merge_geometries_controlled(records, &Control::detached())
}

/// Merges a layer's features as the profile's post-processing does,
/// stopping on a cancel between merge groups.
fn merge_geometries_controlled(
    records: Vec<Record>,
    control: &Control,
) -> Result<Vec<Record>, String> {
    // Water.postProcess returns a one-feature layer unchanged. Running the
    // merge pipeline here would remove small components from that feature.
    if records.len() == 1 && records[0].layer == "water" {
        return Ok(records);
    }
    let mut grouped: BTreeMap<Vec<u8>, Vec<Record>> = BTreeMap::new();
    let mut key_scratch = Vec::new();
    let mut output = vec![];
    for mut record in records {
        if record.layer == "housenumber" {
            record.attrs.retain(|k, _| !k.starts_with('_'));
        }
        let cover_merge = if record.layer == "landcover" {
            let count = record.attrs.remove("_numpoints").and_then(|v| match v {
                AttrValue::Int(v) => Some(v as u64),
                AttrValue::UInt(v) => Some(v),
                _ => None,
            });
            let forest = matches!(record.attrs.get("subclass"),Some(AttrValue::String(s)) if s=="wood"||s=="forest");
            match (record.zoom, count) {
                (7, Some(_)) => true,
                (8 | 9, Some(count)) if forest => {
                    record
                        .attrs
                        .insert("__cover_group".into(), AttrValue::Bool(count < 300));
                    true
                }
                (10..=13, Some(count)) if forest && count < 300 => true,
                _ => false,
            }
        } else {
            false
        };
        let merge = match &record.geometry {
            TileGeometry::Polygon(_) => match &*record.layer {
                "water" | "building" => true,
                "landcover" => cover_merge,
                "landuse" => {
                    matches!(record.attrs.get("class"),Some(AttrValue::String(s)) if s=="residential")
                }
                "park" => record.zoom <= 4,
                _ => false,
            },
            TileGeometry::Line { .. } => {
                matches!(
                    &*record.layer,
                    "transportation" | "transportation_name" | "boundary" | "sar"
                ) || (record.layer == "waterway" && (6..=11).contains(&record.zoom))
            }
            TileGeometry::Point(_) => record.layer == "housenumber",
            _ => false,
        };
        if merge {
            let unique = if record.layer == "transportation"
                && matches!(
                    record.attrs.get("oneway"),
                    Some(AttrValue::Int(1 | -1)) | Some(AttrValue::UInt(1))
                ) {
                Some(record.id)
            } else {
                None
            };
            write_group_key(&mut key_scratch, &record, unique);
            if let Some(group) = grouped.get_mut(key_scratch.as_slice()) {
                group.push(record);
            } else {
                grouped.insert(std::mem::take(&mut key_scratch), vec![record]);
            }
        } else {
            output.push(record);
        }
    }
    for (_, group) in grouped {
        control.check()?;
        let merged_count = group.len();
        let mut group = group.into_iter();
        let mut first = group.next().expect("nonempty merge group");
        if merged_count > 1 {
            first.id = (first.id / 10) * 10;
        }
        match &mut first.geometry {
            TileGeometry::Polygon(groups) => {
                let policy = polygon_merge_policy(&first.layer, first.zoom);
                // Combine retains the original integer rings; floating-point
                // polygon conversion would be allocated and then discarded.
                let dissolve = matches!(policy, PolygonMerge::Dissolve { .. });
                let mut geometries = Vec::new();
                let mut originals = Vec::new();
                let mut original_groups = std::mem::take(groups);
                if dissolve {
                    geometries.push(to_polygons(&original_groups));
                    originals.push(std::mem::take(&mut original_groups));
                }
                for other in group {
                    if let TileGeometry::Polygon(g) = other.geometry {
                        if dissolve {
                            geometries.push(to_polygons(&g));
                            originals.push(g);
                        } else {
                            original_groups.extend(g);
                        }
                    }
                }
                *groups = match policy {
                    PolygonMerge::Combine => original_groups,
                    PolygonMerge::Dissolve {
                        min_area,
                        min_hole_area,
                        distance,
                    } => {
                        #[cfg(feature = "benchmark-profiling")]
                        merge_dump::record(
                            &first.layer,
                            first.zoom,
                            first.tile_id,
                            distance,
                            &originals,
                        );
                        let mut result = Vec::new();
                        let mut cluster_ids = Vec::new();
                        // Experiment: clusters whose output the area filter
                        // below is certain to drop skip union and unbuffer.
                        if std::env::var_os("OTC_NO_DROP_BOUND").is_none() {
                            DROP_BELOW.with(|d| d.set(min_area * 256.));
                        }
                        let clusters = merge_nearby(geometries, distance * 16.);
                        DROP_BELOW.with(|d| d.set(0.));
                        for (cluster_id, (members, merged, jts)) in clusters.into_iter().enumerate()
                        {
                            let rings = if members.len() == 1 {
                                // Pinned FeatureMerge leaves singleton clusters
                                // encoded as-is, without another precision repair.
                                std::mem::take(&mut originals[members[0]])
                            } else if distance > 0. || jts {
                                // A merged cluster whose envelope is below the
                                // minimum area is dropped, the rest snapped as
                                // GeoUtils.snapAndFixPolygon does.
                                let envelope = merged
                                    .bounding_rect()
                                    .map_or(0., |r| r.width() * r.height());
                                if envelope < min_area * 256. {
                                    vec![]
                                } else {
                                    super::jts_buffer::snap_and_fix(&merged)
                                        .map(from_grid_polygons)
                                        .unwrap_or_else(|| from_polygons(merged))
                                }
                            } else {
                                // Its snap/fix and area extraction are scoped to
                                // each proximity cluster, not the entire tile.
                                from_polygons(merged)
                            };
                            let filtered = filter_tile_polygon_areas(
                                rings,
                                min_area * 256.,
                                min_hole_area * 256.,
                            );
                            if distance > 0. {
                                cluster_ids.extend(std::iter::repeat_n(cluster_id, filtered.len()));
                            }
                            result.extend(filtered);
                        }
                        if distance > 0. {
                            repair_cross_cluster_contacts(result, &cluster_ids)
                        } else {
                            result
                        }
                    }
                };
            }
            TileGeometry::Line { paths, scale_exp } => {
                for other in group {
                    if let TileGeometry::Line {
                        paths: other,
                        scale_exp: other_scale,
                    } = other.geometry
                    {
                        if other_scale != *scale_exp {
                            return Err("OMT line merge encountered inconsistent precision".into());
                        }
                        paths.extend(other);
                    }
                }
                // The unpruned line graph must snap and node before degree-two
                // merging, matching the pinned LoopLineMerger stage order.
                // Transportation, its name layer, and waterway use the same graph with their
                // zoom-dependent loop, stub, and final length thresholds.
                let full_graph = matches!(
                    &*first.layer,
                    "boundary" | "transportation" | "transportation_name" | "waterway"
                );
                if !full_graph {
                    *paths = join_paths(std::mem::take(paths));
                }
                snap_line_paths(paths, *scale_exp);
                *scale_exp = 0;
                let tolerance = if first.zoom >= 14 { 1. } else { 1.6 };
                if let Some((minimum, buffer)) =
                    line_merge_policy(&first.layer, first.zoom, &first.attrs)
                {
                    if full_graph {
                        if first.layer == "boundary" || minimum <= 0. {
                            super::line_merge::merge_graph_without_pruning(paths, tolerance);
                        } else {
                            super::line_merge::merge_graph_with_pruning(
                                paths,
                                tolerance,
                                (minimum * 16.).min(f64::MAX),
                            );
                        }
                    } else {
                        simplify_and_filter_lines(paths, tolerance, minimum * 16.);
                    }
                    *paths = paths
                        .iter()
                        .flat_map(|p| remove_detail_outside(p, -buffer * 16., 4096. + buffer * 16.))
                        .collect();
                }
            }
            TileGeometry::Point(points) => {
                for other in group {
                    if let TileGeometry::Point(p) = other.geometry {
                        points.extend(p);
                    }
                }
            }
            _ => {}
        }
        first.attrs.remove("__cover_group");
        output.push(first);
    }
    output.sort_by(order);
    Ok(output)
}

/// Writes the bytes that group records with the same geometry type, attributes
/// and `unique` value. Grouping compares these bytes, so their order decides
/// feature order: they are exactly what bincode 1 (default options) wrote for
/// `(geometry_type_id, attrs, unique)`, which the test
/// `scratch_group_keys_match_original_serialization_after_reuse` checks
/// against bincode itself.
fn write_group_key(scratch: &mut Vec<u8>, record: &Record, unique: Option<u64>) {
    fn length(out: &mut Vec<u8>, len: usize) {
        out.extend_from_slice(&(len as u64).to_le_bytes());
    }
    scratch.clear();
    scratch.push(record.geometry.geometry_type_id());
    length(scratch, record.attrs.len());
    for (key, value) in &record.attrs {
        length(scratch, key.len());
        scratch.extend_from_slice(key.as_bytes());
        // The enum's variant index, as serde's derive numbers them.
        let (variant, payload): (u32, [u8; 8]) = match value {
            AttrValue::String(text) => {
                scratch.extend_from_slice(&0u32.to_le_bytes());
                length(scratch, text.len());
                scratch.extend_from_slice(text.as_bytes());
                continue;
            }
            AttrValue::Int(v) => (1, v.to_le_bytes()),
            AttrValue::UInt(v) => (2, v.to_le_bytes()),
            AttrValue::Double(v) => (3, v.to_bits().to_le_bytes()),
            AttrValue::Bool(v) => {
                scratch.extend_from_slice(&4u32.to_le_bytes());
                scratch.push(u8::from(*v));
                continue;
            }
        };
        scratch.extend_from_slice(&variant.to_le_bytes());
        scratch.extend_from_slice(&payload);
    }
    match unique {
        None => scratch.push(0),
        Some(v) => {
            scratch.push(1);
            scratch.extend_from_slice(&v.to_le_bytes());
        }
    }
}

#[derive(Clone, Copy)]
enum PolygonMerge {
    Combine,
    Dissolve {
        min_area: f64,
        min_hole_area: f64,
        distance: f64,
    },
}
fn polygon_merge_policy(layer: &str, zoom: u8) -> PolygonMerge {
    match layer {
        "building" if zoom == 13 => PolygonMerge::Dissolve {
            min_area: 4.,
            min_hole_area: 4.,
            distance: 0.5,
        },
        "building" => PolygonMerge::Combine,
        "landuse" if zoom >= 13 => PolygonMerge::Combine,
        "landuse" => PolygonMerge::Dissolve {
            min_area: 1.,
            min_hole_area: 1.,
            distance: match zoom {
                0..=5 => 0.1,
                6 => 0.5,
                7 => 0.25,
                8 => 0.125,
                _ => 0.1,
            },
        },
        "landcover" => PolygonMerge::Dissolve {
            min_area: 4.,
            min_hole_area: 0.,
            distance: 0.,
        },
        "park" => PolygonMerge::Dissolve {
            min_area: 0.,
            min_hole_area: 0.,
            distance: 0.,
        },
        _ => PolygonMerge::Dissolve {
            min_area: if zoom == 14 { 0.0625 } else { 1. },
            min_hole_area: 0.,
            distance: 0.,
        },
    }
}

pub(super) fn filter_tile_polygon_areas(
    mut groups: Vec<TileRingGroup>,
    minimum: f64,
    hole_minimum: f64,
) -> Vec<TileRingGroup> {
    let ring_area = |ring: &[TilePoint]| {
        let Some(origin) = ring.first() else {
            return 0.;
        };
        ring.windows(2)
            .map(|edge| {
                (i64::from(edge[0].x) - i64::from(origin.x))
                    * (i64::from(edge[1].y) - i64::from(origin.y))
                    - (i64::from(edge[1].x) - i64::from(origin.x))
                        * (i64::from(edge[0].y) - i64::from(origin.y))
            })
            .sum::<i64>()
            .unsigned_abs() as f64
            / 2.
    };
    groups.retain_mut(|group| {
        // Not `<=`: a NaN comparison must drop the group too.
        let exterior_area = ring_area(&group.exterior);
        let large_enough = exterior_area > minimum;
        if !large_enough {
            return false;
        }
        if hole_minimum > 0. {
            // Pinned extractPolygons compares holes against minArea when its
            // minHoleArea gate is enabled.
            group.holes.retain(|hole| ring_area(hole) > minimum);
        }
        if let Some(fill) = fill_square(exterior_area, &group.exterior) {
            group.exterior = fill;
        }
        true
    });
    groups
}

/// `FeatureMerge.isFill` and `createFill`: an exterior ring that goes round
/// the whole tile, beyond every edge and without crossing the tile, becomes a
/// canonical square overhanging each edge by the same amount, so filled tiles
/// (open sea, a city's residential area) are byte for byte alike. Planetiler
/// works in tile pixels: `area` and the ring's MVT units are divided by 256
/// and 16 (exactly, powers of two), and the square is encoded as
/// `Math.round(x * 4096 / 256)`.
fn fill_square(area: f64, ring: &[TilePoint]) -> Option<Vec<TilePoint>> {
    const LEFT: u8 = 1;
    const RIGHT: u8 = 2;
    const TOP: u8 = 4;
    const BOTTOM: u8 = 8;
    let area = area / 256.;
    if area < 256. * 256. {
        return None;
    }
    let buffer = (area.sqrt() - 256.) / 2.;
    let (min, max) = (-(buffer * 0.9), 256. + buffer * 0.9);
    let side = |p: &TilePoint| {
        let (x, y) = (f64::from(p.x) / 16., f64::from(p.y) / 16.);
        (if x < min {
            LEFT
        } else if x > max {
            RIGHT
        } else {
            0
        }) | (if y < min {
            TOP
        } else if y > max {
            BOTTOM
        } else {
            0
        })
    };
    let mut visited = 0;
    for edge in ring.windows(2) {
        let (a, b) = (side(&edge[0]), side(&edge[1]));
        visited |= a;
        // VectorGeometry.segmentCrossesTile: the segment may enter the tile.
        if a & b == 0 {
            return None;
        }
    }
    if visited != LEFT | RIGHT | TOP | BOTTOM {
        return None;
    }
    // Math.round: the nearest integer, ties towards positive infinity.
    let round = |v: f64| {
        let floor = v.floor();
        (if v - floor >= 0.5 { floor + 1. } else { floor }) as i32
    };
    let (low, high) = (round(-buffer * 16.), round((buffer + 256.) * 16.));
    Some(vec![
        TilePoint::new(low, low),
        TilePoint::new(high, low),
        TilePoint::new(high, high),
        TilePoint::new(low, high),
        TilePoint::new(low, low),
    ])
}

thread_local! {
    /// The exterior area at or below which a merged cluster's polygons are
    /// dropped by the caller; 0 disables the early-out.
    static DROP_BELOW: std::cell::Cell<f64> = const { std::cell::Cell::new(0.) };
}

fn merge_nearby(
    geometries: Vec<MultiPolygon<f64>>,
    distance: f64,
) -> Vec<(Vec<usize>, MultiPolygon<f64>, bool)> {
    #[cfg(test)]
    {
        merge_nearby_impl::<true>(geometries, distance, &mut 0)
    }
    #[cfg(not(test))]
    {
        merge_nearby_impl::<true>(geometries, distance)
    }
}

// The reference mode is exercised by tests against exactly the same buffer and
// union pipeline. Count only actual distance evaluations, not envelope checks.
fn merge_nearby_impl<const SKIP_CONNECTED: bool>(
    geometries: Vec<MultiPolygon<f64>>,
    distance: f64,
    #[cfg(test)] distance_tests: &mut usize,
) -> Vec<(Vec<usize>, MultiPolygon<f64>, bool)> {
    fn find(parent: &mut [usize], mut n: usize) -> usize {
        while parent[n] != n {
            parent[n] = parent[parent[n]];
            n = parent[n];
        }
        n
    }
    let mut parent = (0..geometries.len()).collect::<Vec<_>>();
    let bounds = geometries
        .iter()
        .map(|g| g.bounding_rect())
        .collect::<Vec<_>>();
    let mut order = (0..geometries.len())
        .filter(|i| bounds[*i].is_some())
        .collect::<Vec<_>>();
    order.sort_by(|a, b| {
        bounds[*a]
            .unwrap()
            .min()
            .x
            .total_cmp(&bounds[*b].unwrap().min().x)
    });
    for (offset, &i) in order.iter().enumerate() {
        let a = bounds[i].unwrap();
        for &j in &order[offset + 1..] {
            let b = bounds[j].unwrap();
            if b.min().x > a.max().x + distance {
                break;
            }
            if b.min().y > a.max().y + distance || b.max().y < a.min().y - distance {
                continue;
            }
            let left = find(&mut parent, i);
            let right = find(&mut parent, j);
            // An edge within a connected component cannot change its members,
            // root, or the deterministic order of the subsequent union inputs.
            if SKIP_CONNECTED && left == right {
                continue;
            }
            #[cfg(test)]
            {
                *distance_tests += 1;
            }
            #[cfg(test)]
            let near = if merge_replay::GEO_PROXIMITY.with(std::cell::Cell::get) {
                Euclidean.distance(&geometries[i], &geometries[j]) <= distance
            } else {
                within_distance(&geometries[i], &geometries[j], distance)
            };
            #[cfg(not(test))]
            let near = within_distance(&geometries[i], &geometries[j], distance);
            if near {
                parent[right] = left;
            }
        }
    }
    let mut clusters: BTreeMap<usize, Vec<(usize, MultiPolygon<f64>)>> = BTreeMap::new();
    for (i, g) in geometries.into_iter().enumerate() {
        clusters
            .entry(find(&mut parent, i))
            .or_default()
            .push((i, g));
    }
    let buffer = |g: &MultiPolygon<f64>, amount: f64| {
        g.buffer_with_style(
            BufferStyle::new(amount).line_join(LineJoin::Miter(2. * (1_f64 / 5.).asin())),
        )
    };
    let mut result = Vec::with_capacity(clusters.len());
    for indexed in clusters.into_values() {
        let (members, cluster): (Vec<_>, Vec<_>) = indexed.into_iter().unzip();
        if cluster.len() == 1 {
            result.push((members, cluster.into_iter().next().unwrap(), false));
            continue;
        }
        #[cfg(test)]
        if merge_replay::COLLECT_ONLY.with(std::cell::Cell::get) {
            result.push((members, MultiPolygon(vec![]), false));
            continue;
        }
        let merged = if distance > 0. {
            // FeatureMerge.bufferUnionUnbuffer, through the port of the
            // pinned JTS buffer and union; the integer overlay remains for
            // inputs outside the port's bounds.
            super::jts_buffer::buffer_union_unbuffer(
                &cluster,
                distance,
                DROP_BELOW.with(std::cell::Cell::get),
            )
            .unwrap_or_else(|| overlay_buffer_union_unbuffer(&cluster, distance, buffer))
        } else {
            // Pinned mergeOverlappingPolygons uses minDist=buffer=0: a zero
            // buffer of each connected cluster's collection, through the
            // port of the pinned JTS buffer. Outside the port's bounds, the
            // integer overlay's union with restored source vertices.
            let collection =
                MultiPolygon(cluster.iter().flat_map(|g| g.0.iter().cloned()).collect());
            match super::jts_buffer::buffer(&collection, 0.) {
                Some(merged) => merged,
                None => {
                    let merged = super::float_repair::union_with_provenance(&cluster)
                        .unwrap_or_else(|| unary_union(&cluster));
                    result.push((members, merged, false));
                    continue;
                }
            }
        };
        result.push((members, merged, true));
    }
    result.sort_by_key(|(members, ..)| members[0]);
    result
}

/// Products of vertex counts up to which the proximity test compares every
/// vertex with every segment; above it, it sweeps over vertices sorted by x.
const DIRECT_SCAN_PAIRS: usize = 64 * 64;

/// Whether `Euclidean.distance(a, b) <= distance`, as `merge_nearby` asks it,
/// with an early exit at the first vertex and segment within the distance.
/// geo's distance is the minimum of the same `line_segment_distance` values
/// over the same rings (through R-trees of segments that it builds for every
/// pair, or a sorted projection), so the answers could differ only where two
/// segments tie for a vertex's nearest within rounding of `distance`. Polygons
/// with holes whose boxes overlap still go to geo, which picks their rings by
/// a containment test.
fn within_distance(a: &MultiPolygon<f64>, b: &MultiPolygon<f64>, distance: f64) -> bool {
    // geo folds the minimum over every pair of member polygons.
    a.0.iter()
        .any(|p| b.0.iter().any(|q| polygons_within(p, q, distance)))
}

fn polygons_within(p: &Polygon<f64>, q: &Polygon<f64>, distance: f64) -> bool {
    let (pe, qe) = (p.exterior(), q.exterior());
    if pe.0.is_empty() || qe.0.is_empty() {
        // geo answers 0 for an empty polygon.
        return true;
    }
    let (rp, rq) = (ring_rect(pe), ring_rect(qe));
    let x_separated = rp.max().x < rq.min().x || rq.max().x < rp.min().x;
    let y_separated = rp.max().y < rq.min().y || rq.max().y < rp.min().y;
    if (x_separated || y_separated) && pe.0.len() >= 4 && qe.0.len() >= 4 {
        // Disjoint boxes cannot intersect; geo measures between the exteriors.
        return rings_within(pe, qe, distance);
    }
    if !p.interiors().is_empty() || !q.interiors().is_empty() {
        return Euclidean.distance(p, q) <= distance;
    }
    // geo: 0 if they intersect, else the distance between the exteriors.
    rings_within(pe, qe, distance) || exteriors_intersect(p, q)
}

fn ring_rect(ring: &LineString<f64>) -> Rect<f64> {
    let first = ring.0[0];
    let (mut min, mut max) = (first, first);
    for c in &ring.0[1..] {
        min.x = min.x.min(c.x);
        min.y = min.y.min(c.y);
        max.x = max.x.max(c.x);
        max.y = max.y.max(c.y);
    }
    Rect::new(min, max)
}

/// Whether some vertex of either ring lies within `distance` of a segment of
/// the other, by geo's `line_segment_distance`.
fn rings_within(a: &LineString<f64>, b: &LineString<f64>, distance: f64) -> bool {
    vertices_within(&a.0, &b.0, distance) || vertices_within(&b.0, &a.0, distance)
}

fn vertices_within(points: &[Coord<f64>], ring: &[Coord<f64>], distance: f64) -> bool {
    // A vertex farther than this from a segment's box along one axis cannot
    // be within `distance` of it; the margin covers line_segment_distance's
    // rounding.
    let reach = distance + 1e-6 * (1. + distance);
    let reach_box = |start: Coord<f64>, end: Coord<f64>| {
        (
            start.x.min(end.x) - reach,
            start.x.max(end.x) + reach,
            start.y.min(end.y) - reach,
            start.y.max(end.y) + reach,
        )
    };
    if points.len() * ring.len() <= DIRECT_SCAN_PAIRS {
        for segment in ring.windows(2) {
            let (start, end) = (segment[0], segment[1]);
            let (min_x, max_x, min_y, max_y) = reach_box(start, end);
            for &point in points {
                if point.x < min_x || point.x > max_x || point.y < min_y || point.y > max_y {
                    continue;
                }
                if line_segment_distance(point, start, end) <= distance {
                    return true;
                }
            }
        }
        return false;
    }
    let mut sorted = points.to_vec();
    sorted.sort_unstable_by(|a, b| a.x.total_cmp(&b.x));
    for segment in ring.windows(2) {
        let (start, end) = (segment[0], segment[1]);
        let (min_x, max_x, min_y, max_y) = reach_box(start, end);
        let first = sorted.partition_point(|p| p.x < min_x);
        for &point in &sorted[first..] {
            if point.x > max_x {
                break;
            }
            if point.y < min_y || point.y > max_y {
                continue;
            }
            if line_segment_distance(point, start, end) <= distance {
                return true;
            }
        }
    }
    false
}

/// geo's `Polygon::intersects` for polygons without holes whose boxes
/// overlap: a vertex of either inside or on the other, or crossing exterior
/// segments, by the same robust predicates; large rings are swept by x
/// instead of compared pair by pair.
fn exteriors_intersect(p: &Polygon<f64>, q: &Polygon<f64>) -> bool {
    let (pe, qe) = (&p.exterior().0, &q.exterior().0);
    if q.intersects(&pe[0]) || p.intersects(&qe[0]) {
        return true;
    }
    let boxes_meet = |a: &[Coord<f64>], b: &[Coord<f64>]| {
        a[0].x.min(a[1].x) <= b[0].x.max(b[1].x)
            && b[0].x.min(b[1].x) <= a[0].x.max(a[1].x)
            && a[0].y.min(a[1].y) <= b[0].y.max(b[1].y)
            && b[0].y.min(b[1].y) <= a[0].y.max(a[1].y)
    };
    let cross = |a: &[Coord<f64>], b: &[Coord<f64>]| {
        boxes_meet(a, b) && geo::Line::new(a[0], a[1]).intersects(&geo::Line::new(b[0], b[1]))
    };
    if pe.len() * qe.len() <= DIRECT_SCAN_PAIRS {
        return pe.windows(2).any(|a| qe.windows(2).any(|b| cross(a, b)));
    }
    let mut segments: Vec<&[Coord<f64>]> = qe.windows(2).collect();
    segments.sort_unstable_by(|a, b| a[0].x.min(a[1].x).total_cmp(&b[0].x.min(b[1].x)));
    let widest = segments
        .iter()
        .map(|s| (s[0].x - s[1].x).abs())
        .fold(0_f64, f64::max);
    pe.windows(2).any(|a| {
        let (low, high) = (a[0].x.min(a[1].x), a[0].x.max(a[1].x));
        // Segments starting further left than this end before `low`; the
        // margin covers rounding of the subtraction.
        let from = low - widest - (1. + 1e-9 * low.abs());
        let first = segments.partition_point(|s| s[0].x.min(s[1].x) < from);
        segments[first..]
            .iter()
            .take_while(|s| s[0].x.min(s[1].x) <= high)
            .any(|b| cross(a, b))
    })
}

/// geo-types' `private_utils::line_segment_distance` for f64, with the same
/// floating-point operations.
fn line_segment_distance(point: Coord<f64>, start: Coord<f64>, end: Coord<f64>) -> f64 {
    if start == end {
        return (start.x - point.x).hypot(start.y - point.y);
    }
    let dx = end.x - start.x;
    let dy = end.y - start.y;
    let d_squared = dx * dx + dy * dy;
    let r = ((point.x - start.x) * dx + (point.y - start.y) * dy) / d_squared;
    if r <= 0. {
        return (start.x - point.x).hypot(start.y - point.y);
    }
    if r >= 1. {
        return (end.x - point.x).hypot(end.y - point.y);
    }
    let s = ((start.y - point.y) * dx - (start.x - point.x) * dy) / d_squared;
    s.abs() * dx.hypot(dy)
}

fn overlay_buffer_union_unbuffer(
    cluster: &[MultiPolygon<f64>],
    distance: f64,
    buffer: impl Fn(&MultiPolygon<f64>, f64) -> MultiPolygon<f64>,
) -> MultiPolygon<f64> {
    let expanded = cluster
        .iter()
        .map(|g| buffer(g, distance))
        .collect::<Vec<_>>();
    let mut merged = buffer(&unary_union(&expanded), -distance);
    let largest = cluster
        .iter()
        .map(Area::unsigned_area)
        .fold(0_f64, f64::max);
    if largest > 100. * 256. && merged.unsigned_area() < largest * 0.95 {
        let original = unary_union(cluster);
        let alternate = buffer(&buffer(&original, distance), -distance);
        if alternate.unsigned_area() < largest * 0.95 {
            merged = original;
        } else if alternate.unsigned_area() > merged.unsigned_area() {
            merged = alternate;
        }
    }
    merged
}

fn line_merge_policy(layer: &str, zoom: u8, attrs: &AttrMap) -> Option<(f64, f64)> {
    let pixels = |meters: f64| meters / 40_075_016.685_578_49 * 256. * 2_f64.powi(i32::from(zoom));
    match layer {
        "waterway" if (6..=11).contains(&zoom) => Some((
            if zoom <= 8 {
                1.
            } else {
                super::water::merge_min_length_pixels(zoom).unwrap_or(1.)
            },
            4.,
        )),
        "transportation" => Some((
            pixels(match zoom {
                4 => 1000.,
                5 => 500.,
                6 => 100.,
                7 => 50.,
                _ => 0.,
            }),
            4.,
        )),
        "boundary" => Some((1. / 16., 4.)),
        // Trails, power lines and cliffs with the same attributes join into
        // one line, as roads do at zoom 8 and above.
        "sar" => Some((0., 4.)),
        "transportation_name" => {
            let minimum = match zoom {
                6 | 7 => pixels(20_000.),
                8 => pixels(14_000.),
                9 => pixels(8_000.),
                10 => pixels(4_000.),
                11 => pixels(2_000.),
                12 | 13 => attrs
                    .get("name")
                    .or_else(|| attrs.get("ref"))
                    .and_then(|v| {
                        if let AttrValue::String(v) = v {
                            Some(v.encode_utf16().count() as f64 * 6.)
                        } else {
                            None
                        }
                    })
                    .unwrap_or(f64::MAX),
                _ => 0.,
            };
            Some((minimum, 8.))
        }
        _ => None,
    }
}

fn path_length(points: &[TilePoint]) -> f64 {
    points
        .windows(2)
        .map(|w| {
            ((f64::from(w[1].x) - f64::from(w[0].x)).powi(2)
                + (f64::from(w[1].y) - f64::from(w[0].y)).powi(2))
            .sqrt()
        })
        .sum()
}

fn snap_line_paths(paths: &mut Vec<Vec<TilePoint>>, scale_exp: u8) {
    let scale = 2_f64.powi(i32::from(scale_exp));
    for path in paths.iter_mut() {
        if scale_exp != 0 {
            for p in path.iter_mut() {
                // JTS PrecisionModel rounds half ties toward positive infinity.
                p.x = (f64::from(p.x) / scale + 0.5).floor() as i32;
                p.y = (f64::from(p.y) / scale + 0.5).floor() as i32;
            }
        }
        path.dedup();
    }
    paths.retain(|p| p.len() >= 2);
}

fn simplify_and_filter_lines(paths: &mut Vec<Vec<TilePoint>>, tolerance: f64, minimum: f64) {
    // LoopLineMerger keeps Edge.length from the snapped, unsimplified graph.
    // Recomputing it after DP incorrectly drops names near the length cutoff.
    let retain: Vec<_> = if minimum > 0. {
        paths.iter().map(|p| path_length(p) >= minimum).collect()
    } else {
        vec![]
    };
    simplify_merged_lines(paths, tolerance);
    if minimum > 0. {
        let mut i = 0;
        paths.retain(|_| {
            let keep = retain[i];
            i += 1;
            keep
        });
    }
}

fn simplify_merged_lines(paths: &mut [Vec<TilePoint>], tolerance: f64) {
    // Shared interior vertices (including a loop's return point) must become
    // graph endpoints before simplification, or a branch can be disconnected.
    let mut counts = BTreeMap::<TilePoint, usize>::new();
    for point in paths.iter().flatten() {
        *counts.entry(*point).or_default() += 1;
    }
    for path in paths {
        let mut result = Vec::new();
        let mut start = 0;
        for i in 1..path.len().saturating_sub(1) {
            if counts[&path[i]] > 1 {
                let mut segment = path[start..=i].to_vec();
                simplify_merged_line(&mut segment, tolerance);
                segment.pop();
                result.extend(segment);
                start = i;
            }
        }
        if start == 0 {
            simplify_merged_line(path, tolerance);
        } else {
            let mut segment = path[start..].to_vec();
            simplify_merged_line(&mut segment, tolerance);
            result.extend(segment);
            *path = result;
        }
    }
}

fn simplify_merged_line(points: &mut Vec<TilePoint>, tolerance: f64) {
    if points.len() <= 2 {
        return;
    }
    // Iterative Douglas-Peucker avoids recursion proportional to input size.
    // Strict comparison preserves the first farthest point and drops points
    // exactly at tolerance, matching the pinned line-merger policy.
    let mut keep = vec![false; points.len()];
    keep[0] = true;
    keep[points.len() - 1] = true;
    let mut pending = vec![(0, points.len() - 1)];
    while let Some((first, last)) = pending.pop() {
        let a = points[first];
        let b = points[last];
        let (dx, dy) = (
            f64::from(b.x) - f64::from(a.x),
            f64::from(b.y) - f64::from(a.y),
        );
        let mut maximum = tolerance * tolerance;
        let mut selected = None;
        for (i, p) in points.iter().enumerate().take(last).skip(first + 1) {
            let (mut x, mut y) = (f64::from(a.x), f64::from(a.y));
            if dx != 0. || dy != 0. {
                let t =
                    ((f64::from(p.x) - x) * dx + (f64::from(p.y) - y) * dy) / (dx * dx + dy * dy);
                if t > 1. {
                    x = f64::from(b.x);
                    y = f64::from(b.y);
                } else if t > 0. {
                    x += dx * t;
                    y += dy * t;
                }
            }
            let (px, py) = (f64::from(p.x) - x, f64::from(p.y) - y);
            let distance = px * px + py * py;
            if distance > maximum {
                maximum = distance;
                selected = Some(i);
            }
        }
        if let Some(i) = selected {
            keep[i] = true;
            if i > first + 1 {
                pending.push((first, i));
            }
            if last > i + 1 {
                pending.push((i, last));
            }
        }
    }
    let mut i = 0;
    points.retain(|_| {
        let retained = keep[i];
        i += 1;
        retained
    });
}

fn join_paths(mut paths: Vec<Vec<TilePoint>>) -> Vec<Vec<TilePoint>> {
    // Endpoint adjacency, preserving branch nodes rather than arbitrarily pairing junctions.
    let mut ends: BTreeMap<TilePoint, Vec<usize>> = BTreeMap::new();
    for (i, p) in paths.iter().enumerate() {
        if p.len() > 1 {
            ends.entry(p[0]).or_default().push(i);
            ends.entry(*p.last().unwrap()).or_default().push(i);
        }
    }
    let mut output = vec![];
    for index in 0..paths.len() {
        let mut line = std::mem::take(&mut paths[index]);
        if line.len() < 2 {
            continue;
        }
        for _ in 0..2 {
            loop {
                let endpoint = *line.last().unwrap();
                let Some(neighbors) = ends.get(&endpoint).filter(|v| v.len() == 2) else {
                    break;
                };
                let Some(next) = neighbors.iter().copied().find(|i| !paths[*i].is_empty()) else {
                    break;
                };
                let mut continuation = std::mem::take(&mut paths[next]);
                if continuation[0] != endpoint {
                    continuation.reverse();
                }
                line.extend(continuation.into_iter().skip(1));
            }
            line.reverse();
        }
        output.push(line);
    }
    output
}

fn remove_detail_outside(points: &[TilePoint], min: f64, max: f64) -> Vec<Vec<TilePoint>> {
    if points.len() < 2 {
        return vec![];
    }
    let mut result = vec![];
    let mut current = vec![];
    let mut was_in = false;
    for pair in points.windows(2) {
        let now_in = f64::from(pair[0].x.min(pair[1].x)) <= max
            && f64::from(pair[0].x.max(pair[1].x)) >= min
            && f64::from(pair[0].y.min(pair[1].y)) <= max
            && f64::from(pair[0].y.max(pair[1].y)) >= min;
        if now_in || was_in {
            current.push(pair[0]);
        } else if !current.is_empty() {
            if current.len() >= 2 {
                result.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
        was_in = now_in;
    }
    let last = *points.last().unwrap();
    if was_in
        || (f64::from(last.x) >= min
            && f64::from(last.x) <= max
            && f64::from(last.y) >= min
            && f64::from(last.y) <= max)
    {
        current.push(last);
    }
    if current.len() >= 2 {
        result.push(current);
    }
    result
}

#[allow(dead_code)]
fn clip_path(points: &[TilePoint], min: f64, max: f64) -> Vec<Vec<TilePoint>> {
    let mut output = vec![];
    let mut current = vec![];
    for pair in points.windows(2) {
        let (x, y) = (f64::from(pair[0].x), f64::from(pair[0].y));
        let (dx, dy) = (f64::from(pair[1].x) - x, f64::from(pair[1].y) - y);
        let (mut lo, mut hi) = (0.0_f64, 1.0_f64);
        let mut keep = true;
        for (p, q) in [(-dx, x - min), (dx, max - x), (-dy, y - min), (dy, max - y)] {
            if p == 0. {
                if q < 0. {
                    keep = false;
                    break;
                }
            } else {
                let t = q / p;
                if p < 0. {
                    lo = lo.max(t);
                } else {
                    hi = hi.min(t);
                }
                if lo > hi {
                    keep = false;
                    break;
                }
            }
        }
        if !keep {
            if current.len() > 1 {
                output.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
            continue;
        }
        let a = TilePoint::new((x + lo * dx).round() as i32, (y + lo * dy).round() as i32);
        let b = TilePoint::new((x + hi * dx).round() as i32, (y + hi * dy).round() as i32);
        if current.last().is_some_and(|v| *v != a) {
            if current.len() > 1 {
                output.push(std::mem::take(&mut current));
            } else {
                current.clear();
            }
        }
        if current.is_empty() {
            current.push(a);
        }
        if current.last() != Some(&b) {
            current.push(b);
        }
    }
    if current.len() > 1 {
        output.push(current);
    }
    output
}

fn to_polygons(groups: &[TileRingGroup]) -> MultiPolygon<f64> {
    let line = |points: &[TilePoint]| {
        LineString(
            points
                .iter()
                .map(|p| Coord {
                    x: f64::from(p.x),
                    y: f64::from(p.y),
                })
                .collect(),
        )
    };
    MultiPolygon(
        groups
            .iter()
            .map(|g| Polygon::new(line(&g.exterior), g.holes.iter().map(|p| line(p)).collect()))
            .collect(),
    )
}

// Positive-distance dissolve is performed per source-proximity cluster. Snapping
// two independently valid clusters can nevertheless leave a small overlap or a
// shared boundary segment in the final MultiPolygon. Repair only those touching
// cross-cluster components; all other rings retain their exact order and bytes.
fn repair_cross_cluster_contacts(
    groups: Vec<TileRingGroup>,
    cluster_ids: &[usize],
) -> Vec<TileRingGroup> {
    let count = groups.len();
    if count < 2 || cluster_ids.len() != count {
        return groups;
    }
    let polygons = to_polygons(&groups).0;
    let bounds = polygons
        .iter()
        .map(BoundingRect::bounding_rect)
        .collect::<Vec<_>>();
    let mut order = (0..count).collect::<Vec<_>>();
    order.sort_by(|&a, &b| {
        bounds[a]
            .as_ref()
            .map(|r| r.min().x)
            .unwrap_or(f64::INFINITY)
            .total_cmp(
                &bounds[b]
                    .as_ref()
                    .map(|r| r.min().x)
                    .unwrap_or(f64::INFINITY),
            )
    });
    let mut parent = (0..count).collect::<Vec<_>>();
    fn root(parent: &mut [usize], mut index: usize) -> usize {
        while parent[index] != index {
            parent[index] = parent[parent[index]];
            index = parent[index];
        }
        index
    }
    let mut has_contact = false;
    for (position, &a) in order.iter().enumerate() {
        let Some(ra) = bounds[a] else { continue };
        for &b in &order[position + 1..] {
            let Some(rb) = bounds[b] else { continue };
            if rb.min().x > ra.max().x {
                break;
            }
            if cluster_ids[a] == cluster_ids[b]
                || rb.min().y > ra.max().y
                || ra.min().y > rb.max().y
            {
                continue;
            }
            let relation = polygons[a].relate(&polygons[b]);
            let overlap =
                relation.get(CoordPos::Inside, CoordPos::Inside) == Dimensions::TwoDimensional;
            let shared_edge = relation.get(CoordPos::OnBoundary, CoordPos::OnBoundary)
                == Dimensions::OneDimensional;
            if overlap || shared_edge {
                let ra = root(&mut parent, a);
                let rb = root(&mut parent, b);
                parent[rb] = ra;
                has_contact = true;
            }
        }
    }
    if !has_contact {
        return groups;
    }
    let mut components = BTreeMap::<usize, Vec<usize>>::new();
    for index in 0..count {
        components
            .entry(root(&mut parent, index))
            .or_default()
            .push(index);
    }
    let mut replacements = BTreeMap::<usize, Vec<TileRingGroup>>::new();
    let mut skip = vec![false; count];
    for members in components.into_values().filter(|members| members.len() > 1) {
        let candidate =
            super::repair::repair_groups(members.iter().map(|&i| groups[i].clone()).collect());
        if candidate.is_empty() || !super::polygon_validity::is_valid(&to_polygons(&candidate)) {
            continue;
        }
        let first = members[0];
        for &index in &members[1..] {
            skip[index] = true;
        }
        replacements.insert(first, candidate);
    }
    let mut output = Vec::with_capacity(count);
    let original = groups.clone();
    for (index, group) in groups.into_iter().enumerate() {
        if let Some(repaired) = replacements.remove(&index) {
            output.extend(repaired);
        } else if !skip[index] {
            output.push(group);
        }
    }
    // A repaired component can round into an otherwise untouched neighbor.
    // Accept the local substitutions only if the final collection is valid.
    if super::polygon_validity::is_valid(&to_polygons(&output)) {
        output
    } else {
        original
    }
}

/// Polygons already on the MVT grid, in tile ring orientation.
pub(super) fn from_grid_polygons(polygons: MultiPolygon<f64>) -> Vec<TileRingGroup> {
    let ring = |line: &LineString<f64>, positive: bool| {
        let mut p = line
            .0
            .iter()
            .map(|c| TilePoint::new(c.x as i32, c.y as i32))
            .collect::<Vec<_>>();
        p.dedup();
        let area = p
            .windows(2)
            .map(|w| i64::from(w[0].x) * i64::from(w[1].y) - i64::from(w[1].x) * i64::from(w[0].y))
            .sum::<i64>();
        if area != 0 && (area > 0) != positive {
            p.reverse();
        }
        p
    };
    polygons
        .0
        .iter()
        .map(|p| TileRingGroup {
            exterior: ring(p.exterior(), true),
            holes: p.interiors().iter().map(|h| ring(h, false)).collect(),
        })
        .filter(|g| g.exterior.len() >= 4)
        .collect()
}

fn from_polygons(polygons: MultiPolygon<f64>) -> Vec<TileRingGroup> {
    fn ring(line: &LineString<f64>, positive: bool) -> Vec<TilePoint> {
        let mut p = line
            .0
            .iter()
            // Match the pinned JTS PrecisionModel at negative buffer coordinates:
            // half ties round toward positive infinity, as in the renderer.
            .map(|c| TilePoint::new((c.x + 0.5).floor() as i32, (c.y + 0.5).floor() as i32))
            .collect::<Vec<_>>();
        p.dedup();
        let area = p
            .windows(2)
            .map(|w| i64::from(w[0].x) * i64::from(w[1].y) - i64::from(w[1].x) * i64::from(w[0].y))
            .sum::<i64>();
        if area == 0 {
            return vec![];
        }
        if (area > 0) != positive {
            p.reverse();
        }
        p
    }
    let groups = polygons
        .0
        .into_iter()
        .filter(|p| p.unsigned_area() > 0.)
        .filter_map(|p| {
            let exterior = ring(p.exterior(), true);
            if exterior.len() < 4 {
                return None;
            }
            Some(TileRingGroup {
                exterior,
                holes: p
                    .interiors()
                    .iter()
                    .map(|h| ring(h, false))
                    .filter(|h| h.len() >= 4)
                    .collect(),
            })
        })
        .collect();
    super::repair::repair_groups(groups)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;
    #[test]
    fn actual_landuse_cross_cluster_overlap_repairs_only_contacting_component() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landuse-18255700-cross-cluster.json"
        ))
        .unwrap();
        let mut groups = fixture["groups"]
            .as_array()
            .unwrap()
            .iter()
            .map(|entry| {
                serde_json::from_value::<TileRingGroup>(entry["geometry"].clone()).unwrap()
            })
            .collect::<Vec<_>>();
        assert_eq!(groups.len(), 2);
        let before_area = to_polygons(&groups).unsigned_area();
        assert!(groups.iter().all(
            |group| super::super::polygon_validity::is_valid(&to_polygons(std::slice::from_ref(
                group
            )))
        ));
        assert!(!super::super::polygon_validity::is_valid(&to_polygons(
            &groups
        )));
        let untouched = TileRingGroup {
            exterior: vec![(0, 0), (40, 0), (40, 40), (0, 40), (0, 0)]
                .into_iter()
                .map(|(x, y)| TilePoint::new(x, y))
                .collect(),
            holes: vec![
                vec![(10, 10), (10, 20), (20, 20), (20, 10), (10, 10)]
                    .into_iter()
                    .map(|(x, y)| TilePoint::new(x, y))
                    .collect(),
            ],
        };
        groups.insert(1, untouched.clone());
        let repaired = repair_cross_cluster_contacts(groups, &[8, 999, 211]);
        assert!(super::super::polygon_validity::is_valid(&to_polygons(
            &repaired
        )));
        let repaired_pair_area = to_polygons(&[repaired[0].clone()]).unsigned_area();
        assert!((before_area - repaired_pair_area).abs() < 2.);
        assert_eq!(repaired[1], untouched);
        assert_eq!(repaired[1].holes.len(), 1);
        assert!(repaired.iter().all(|group| {
            group
                .exterior
                .windows(2)
                .map(|w| {
                    i64::from(w[0].x) * i64::from(w[1].y) - i64::from(w[1].x) * i64::from(w[0].y)
                })
                .sum::<i64>()
                > 0
                && group.holes.iter().all(|hole| {
                    hole.windows(2)
                        .map(|w| {
                            i64::from(w[0].x) * i64::from(w[1].y)
                                - i64::from(w[1].x) * i64::from(w[0].y)
                        })
                        .sum::<i64>()
                        < 0
                })
        }));
    }

    #[test]
    fn cross_cluster_guard_keeps_valid_point_contacts_and_disjoint_holes_exactly() {
        let square = |x: i32, y: i32| TileRingGroup {
            exterior: vec![(x, y), (x + 40, y), (x + 40, y + 40), (x, y + 40), (x, y)]
                .into_iter()
                .map(|(x, y)| TilePoint::new(x, y))
                .collect(),
            holes: vec![],
        };
        let mut holed = square(100, 100);
        holed.holes.push(
            vec![(110, 110), (110, 120), (120, 120), (120, 110), (110, 110)]
                .into_iter()
                .map(|(x, y)| TilePoint::new(x, y))
                .collect(),
        );
        let groups = vec![square(0, 0), square(40, 40), holed];
        assert!(super::super::polygon_validity::is_valid(&to_polygons(
            &groups
        )));
        assert_eq!(
            repair_cross_cluster_contacts(groups.clone(), &[0, 1, 2]),
            groups
        );
        assert_eq!(
            repair_cross_cluster_contacts(groups.clone(), &[0, 0, 2]),
            groups
        );
    }

    #[test]
    fn cross_cluster_shared_edge_repair_keeps_unrelated_hole() {
        let ring = |points: &[(i32, i32)]| {
            points
                .iter()
                .map(|&(x, y)| TilePoint::new(x, y))
                .collect::<Vec<_>>()
        };
        let left = TileRingGroup {
            exterior: ring(&[(0, 0), (20, 0), (20, 20), (0, 20), (0, 0)]),
            holes: vec![],
        };
        let right = TileRingGroup {
            exterior: ring(&[(20, 0), (40, 0), (40, 20), (20, 20), (20, 0)]),
            holes: vec![],
        };
        let untouched = TileRingGroup {
            exterior: ring(&[(100, 100), (140, 100), (140, 140), (100, 140), (100, 100)]),
            holes: vec![ring(&[
                (110, 110),
                (110, 120),
                (120, 120),
                (120, 110),
                (110, 110),
            ])],
        };
        let groups = vec![left, untouched.clone(), right];
        assert!(!super::super::polygon_validity::is_valid(&to_polygons(
            &groups
        )));
        let result = repair_cross_cluster_contacts(groups, &[0, 2, 1]);
        assert!(super::super::polygon_validity::is_valid(&to_polygons(
            &result
        )));
        assert_eq!(result[1], untouched);
    }

    #[test]
    fn actual_forest_union_preserves_reference_collinear_vertices() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landcover-union-1940350350.json"
        ))
        .unwrap();
        let mut records: Vec<Record> = serde_json::from_value(fixture["input"].clone()).unwrap();
        // Captured after the group predicate consumed _numpoints. All six
        // records already passed the z13 forest <300-point predicate.
        for record in &mut records {
            record.attrs.insert("_numpoints".into(), AttrValue::Int(0));
        }
        let expected: Vec<TileRingGroup> =
            serde_json::from_value(fixture["expected"].clone()).unwrap();
        let merged = merge_geometries(records).unwrap();
        assert_eq!(merged.len(), 1);
        let TileGeometry::Polygon(actual) = &merged[0].geometry else {
            panic!("polygon");
        };
        let canonical_ring = |points: &[TilePoint]| {
            let open = &points[..points.len() - 1];
            let mut candidates = Vec::new();
            for reverse in [false, true] {
                let mut ring = open.to_vec();
                if reverse {
                    ring.reverse();
                }
                for i in 0..ring.len() {
                    candidates.push(
                        ring[i..]
                            .iter()
                            .chain(&ring[..i])
                            .copied()
                            .collect::<Vec<_>>(),
                    );
                }
            }
            candidates.into_iter().min().unwrap()
        };
        let canonical = |groups: &[TileRingGroup]| {
            let mut result = groups
                .iter()
                .map(|g| {
                    let mut holes = g
                        .holes
                        .iter()
                        .map(|r| canonical_ring(r))
                        .collect::<Vec<_>>();
                    holes.sort();
                    (canonical_ring(&g.exterior), holes)
                })
                .collect::<Vec<_>>();
            result.sort();
            result
        };
        assert_eq!(canonical(actual), canonical(&expected));
    }
    #[test]
    fn lone_water_feature_keeps_small_components() {
        let mut record = polygon_record("water", 11, 1232, 0);
        record.geometry = TileGeometry::Polygon(vec![TileRingGroup {
            exterior: vec![
                TilePoint::new(0, 0),
                TilePoint::new(8, 0),
                TilePoint::new(8, 8),
                TilePoint::new(0, 8),
                TilePoint::new(0, 0),
            ],
            holes: vec![],
        }]);
        let expected = record.geometry.clone();
        let output = merge_geometries(vec![record]).unwrap();
        assert_eq!(output.len(), 1);
        assert_eq!(output[0].geometry, expected);
        assert_eq!(output[0].id, 1232);
    }
    #[test]
    fn water_singleton_postmerge_retains_actual_collinear_ring_vertex() {
        use geo_types::Geometry;
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-water-collinear-833626865.json"
        ))
        .unwrap();
        let coordinates = fixture["coordinates"]
            .as_array()
            .unwrap()
            .iter()
            .map(|point| (point[0].as_f64().unwrap(), point[1].as_f64().unwrap()))
            .collect::<Vec<_>>();
        let source = super::super::types::Source {
            id: 8336268652,
            osm_id: 833626865,
            kind: super::super::types::SourceKind::Way,
            source_layer: "osm".into(),
            tags: BTreeMap::from([("natural".into(), "water".into())]),
            geometry: Geometry::Polygon(Polygon::new(LineString::from(coordinates), vec![])),
            relations: vec![],
        };
        let mut features = Vec::new();
        super::super::water::State::default().process(&source, &mut features);
        let feature = features
            .into_iter()
            .find(|feature| feature.layer == "water" && feature.min_zoom == 12)
            .unwrap();
        let tile = pmtiles::TileId::from(pmtiles::TileCoord::new(12, 2272, 1389).unwrap()).value();
        let mut records = Vec::new();
        super::super::render::emit_with_record_sink(
            feature,
            &CompileBounds::new(51., 49., 21., 18.).into(),
            12,
            12,
            &mut |record| {
                records.push(record);
                Ok(())
            },
        )
        .unwrap();
        let input = records
            .into_iter()
            .find(|record| record.tile_id == tile)
            .unwrap();
        let expected = input.geometry.clone();
        let merged = merge_geometries(vec![input]).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].geometry, expected);
    }

    #[test]
    fn rings_round_the_whole_tile_become_the_canonical_fill_square() {
        let ring = |points: &[(i32, i32)]| {
            points
                .iter()
                .map(|&(x, y)| TilePoint::new(x, y))
                .collect::<Vec<_>>()
        };
        // An ocean polygon from a 4-pixel buffer (64 MVT units) with collinear
        // nodes left on two edges by the union, as Pomorskie's open sea tiles
        // had: 7 points become Planetiler's 5.
        let sea = TileRingGroup {
            exterior: ring(&[
                (-64, -64),
                (2000, -64),
                (4160, -64),
                (4160, 4160),
                (-64, 4160),
                (-64, 1000),
                (-64, -64),
            ]),
            holes: vec![ring(&[
                (100, 100),
                (100, 200),
                (200, 200),
                (200, 100),
                (100, 100),
            ])],
        };
        let kept = filter_tile_polygon_areas(vec![sea.clone()], 256., 0.);
        assert_eq!(
            kept[0].exterior,
            ring(&[
                (-64, -64),
                (4160, -64),
                (4160, 4160),
                (-64, 4160),
                (-64, -64)
            ])
        );
        assert_eq!(kept[0].holes, sea.holes, "holes are kept");
        // The overhang comes from the area: a ring slightly inside the
        // buffer on one side still fills, with a square of the same area.
        let uneven = TileRingGroup {
            exterior: ring(&[
                (-64, -60),
                (4160, -60),
                (4160, 4160),
                (-64, 4160),
                (-64, -60),
            ]),
            holes: vec![],
        };
        let square = &filter_tile_polygon_areas(vec![uneven], 256., 0.)[0].exterior;
        // 4224 by 4220 units is 69,630 px², a 263.875 px square: 3.9375 px
        // (63 units) over each edge.
        assert_eq!(square[0], TilePoint::new(-63, -63));
        assert_eq!(square[2], TilePoint::new(4159, 4159));
        // Not a fill: the ring dips into the tile on its right side.
        let notched = TileRingGroup {
            exterior: ring(&[
                (-64, -64),
                (4160, -64),
                (4160, 2000),
                (3000, 2048),
                (4160, 2100),
                (4160, 4160),
                (-64, 4160),
                (-64, -64),
            ]),
            holes: vec![],
        };
        assert_eq!(
            filter_tile_polygon_areas(vec![notched.clone()], 256., 0.)[0].exterior,
            notched.exterior
        );
        // Not a fill: smaller than the tile.
        let small = TileRingGroup {
            exterior: ring(&[(0, 0), (4000, 0), (4000, 4000), (0, 4000), (0, 0)]),
            holes: vec![],
        };
        assert_eq!(
            filter_tile_polygon_areas(vec![small.clone()], 256., 0.)[0].exterior,
            small.exterior
        );
    }

    #[test]
    fn polygon_singletons_preserve_encoded_rings_and_zero_distance_clusters() {
        let ring = |x: i32| {
            vec![
                TilePoint::new(x, 0),
                TilePoint::new(x + 32, 0),
                TilePoint::new(x + 32, 32),
                TilePoint::new(x, 32),
                TilePoint::new(x, 0),
            ]
        };
        let first = TileRingGroup {
            exterior: ring(100),
            holes: vec![ring(108)],
        };
        let second = TileRingGroup {
            exterior: ring(200),
            holes: vec![],
        };
        let mut records = Vec::new();
        for (id, geometry) in [(11, first.clone()), (22, second.clone())] {
            let mut item = record(id);
            item.layer = "water".into();
            item.geometry = TileGeometry::Polygon(vec![geometry]);
            records.push(item);
        }
        let merged = merge_geometries(records).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, 10);
        assert_eq!(
            merged[0].geometry,
            TileGeometry::Polygon(vec![first, second])
        );

        // Pinned mergeOverlappingPolygons uses minDist=0; touching polygons
        // must be connected, while distant ones remain singleton clusters.
        let clusters = merge_nearby(
            vec![
                to_polygons(&[TileRingGroup {
                    exterior: ring(0),
                    holes: vec![],
                }]),
                to_polygons(&[TileRingGroup {
                    exterior: ring(32),
                    holes: vec![],
                }]),
                to_polygons(&[TileRingGroup {
                    exterior: ring(100),
                    holes: vec![],
                }]),
            ],
            0.,
        );
        assert_eq!(clusters.len(), 2);
        assert_eq!(clusters[0].0, vec![0, 1]);
        assert_eq!(clusters[1].0, vec![2]);
    }

    #[test]
    fn captured_landuse_merge_replays_ordered_257_source_records() {
        let records: Vec<Record> = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landuse-202730640-premerge.json"
        ))
        .unwrap();
        assert_eq!(records.len(), 257);
        let clusters = merge_nearby(
            records
                .iter()
                .map(|record| match &record.geometry {
                    TileGeometry::Polygon(groups) => to_polygons(groups),
                    _ => panic!("captured input must be polygon"),
                })
                .collect(),
            1.6,
        );
        // Pinned FeatureMerge.groupPolygonsByProximity gives 213 clusters,
        // including 185 singletons, on these exact rendered input records.
        assert_eq!(clusters.len(), 213);
        assert_eq!(
            clusters
                .iter()
                .filter(|(members, ..)| members.len() == 1)
                .count(),
            185
        );
        let merged = merge_geometries(records).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, 202730640);
        let TileGeometry::Polygon(groups) = &merged[0].geometry else {
            panic!("landuse merge must produce polygons");
        };
        assert!(groups.iter().all(|g| g.exterior.len() >= 4));
        assert!(
            groups
                .iter()
                .all(|g| g.exterior.first() == g.exterior.last())
        );
        // The pinned merge of these records, which is also the reference
        // archive's feature: 205 components, two holes, 2005 ring points
        // and 1,217,277 MVT units squared.
        let area2 = |r: &[TilePoint]| {
            r.windows(2)
                .map(|w| {
                    i64::from(w[0].x) * i64::from(w[1].y) - i64::from(w[1].x) * i64::from(w[0].y)
                })
                .sum::<i64>()
        };
        assert_eq!(groups.len(), 205);
        assert_eq!(groups.iter().map(|g| g.holes.len()).sum::<usize>(), 2);
        assert_eq!(groups.iter().map(|g| g.exterior.len()).sum::<usize>(), 2005);
        assert_eq!(
            groups
                .iter()
                .map(|g| area2(&g.exterior) + g.holes.iter().map(|h| area2(h)).sum::<i64>())
                .sum::<i64>(),
            2 * 1_217_277
        );
    }

    #[test]
    fn merged_clusters_match_pinned_feature_merge() {
        // Each case exercises one JTS behaviour: the union's ring start, which
        // the erosion's input simplification never deletes; BufferOp's
        // reduced-precision retry; snapAndFixPolygon's precision reducer with
        // a hole that touches its shell; an EdgeEndStar end replaced by a
        // collinear one, which makes JTS retry the erosion; the zero-buffer
        // repair of an invalid erosion before the precision reducer; a
        // collapsed vertex made a node by SegmentNodeList; the limited mitre
        // join's bevel; and the zero-distance buffer of landcover, whose
        // pointwise rounding leaves a collinear ring that JTS rejects.
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-merge-jts-clusters.json"
        ))
        .unwrap();
        let normalize = |rings: Vec<Vec<(i32, i32)>>| {
            let mut rings = rings
                .into_iter()
                .enumerate()
                .map(|(i, mut ring)| {
                    if ring.first() == ring.last() {
                        ring.pop();
                    }
                    let area = (0..ring.len())
                        .map(|k| {
                            let (a, b) = (ring[k], ring[(k + 1) % ring.len()]);
                            i64::from(a.0) * i64::from(b.1) - i64::from(b.0) * i64::from(a.1)
                        })
                        .sum::<i64>();
                    if (area > 0) != (i == 0) {
                        ring.reverse();
                    }
                    let start = (0..ring.len()).min_by_key(|&k| ring[k]).unwrap();
                    ring.rotate_left(start);
                    ring
                })
                .collect::<Vec<_>>();
            rings[1..].sort();
            rings
        };
        for case in fixture["cases"].as_array().unwrap() {
            let records: Vec<Record> = serde_json::from_value(case["records"].clone()).unwrap();
            let merged = merge_geometries(records).unwrap();
            assert_eq!(merged.len(), 1, "{}", case["name"]);
            let TileGeometry::Polygon(groups) = &merged[0].geometry else {
                panic!("merged cluster must be polygonal");
            };
            let mut actual = groups
                .iter()
                .map(|g| {
                    normalize(
                        std::iter::once(&g.exterior)
                            .chain(&g.holes)
                            .map(|r| r.iter().map(|p| (p.x, p.y)).collect())
                            .collect(),
                    )
                })
                .collect::<Vec<_>>();
            let mut expected = case["expected"]
                .as_array()
                .unwrap()
                .iter()
                .map(|polygon| {
                    let rings: Vec<Vec<[i32; 2]>> =
                        serde_json::from_value(polygon.clone()).unwrap();
                    normalize(
                        rings
                            .into_iter()
                            .map(|r| r.into_iter().map(|p| (p[0], p[1])).collect())
                            .collect(),
                    )
                })
                .collect::<Vec<_>>();
            actual.sort();
            expected.sort();
            assert_eq!(actual, expected, "{}", case["name"]);
        }
    }

    #[test]
    fn merged_polygon_rounding_matches_jts_at_negative_half_coordinates() {
        let polygon = Polygon::new(
            LineString::from(vec![
                (-0.5, -0.5),
                (15.5, -0.5),
                (15.5, 15.5),
                (-0.5, 15.5),
                (-0.5, -0.5),
            ]),
            vec![],
        );
        let groups = from_polygons(MultiPolygon(vec![polygon]));
        assert_eq!(groups.len(), 1);
        assert!(groups[0].holes.is_empty());
        assert_eq!(groups[0].exterior.len(), 5);
        for point in [
            TilePoint::new(0, 0),
            TilePoint::new(16, 0),
            TilePoint::new(16, 16),
            TilePoint::new(0, 16),
        ] {
            assert!(groups[0].exterior.contains(&point));
        }
    }

    #[test]
    fn scratch_group_keys_match_original_serialization_after_reuse() {
        let mut scratch = vec![255; 4096];
        for unique in [None, Some(0), Some(u64::MAX)] {
            for variant in 0..4 {
                let mut r = record(12);
                r.attrs.clear();
                if variant != 0 {
                    r.attrs.insert(
                        "text".into(),
                        AttrValue::String("żółć".repeat(variant * 32)),
                    );
                    r.attrs.insert("signed".into(), AttrValue::Int(i64::MIN));
                    r.attrs.insert("unsigned".into(), AttrValue::UInt(u64::MAX));
                    r.attrs.insert("flag".into(), AttrValue::Bool(true));
                    r.attrs.insert("zero".into(), AttrValue::Double(-0.));
                    r.attrs.insert(
                        "nan".into(),
                        AttrValue::Double(f64::from_bits(0x7ff8_0000_0000_0001)),
                    );
                }
                let expected =
                    bincode::serialize(&(r.geometry.geometry_type_id(), &r.attrs, unique)).unwrap();
                write_group_key(&mut scratch, &r, unique);
                assert_eq!(scratch, expected);
            }
        }
    }
    #[test]
    fn merged_polygon_area_threshold_uses_snapped_integer_rings() {
        let square = |x: f64, side: f64| {
            Polygon::new(
                LineString::from(vec![
                    (x, 0.),
                    (x + side, 0.),
                    (x + side, side),
                    (x, side),
                    (x, 0.),
                ]),
                vec![],
            )
        };
        // Pinned Java PolygonAreaOrderOracle: the first shell has floating
        // area > 256, but JTS precision snapping gives exactly 16*16 = 256.
        let polygons = MultiPolygon(vec![square(0., 16.01), square(40., 17.)]);
        assert!(polygons.0[0].unsigned_area() > 256.);
        let snapped = from_polygons(polygons);
        assert_eq!(snapped.len(), 2);
        let kept = filter_tile_polygon_areas(snapped, 256., 0.);
        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].exterior[0].x, 40);

        // The upstream minHoleArea gate uses minArea for the actual hole
        // comparison. A hole exactly at the strict boundary must be removed.
        let shell = vec![
            TilePoint::new(0, 0),
            TilePoint::new(50, 0),
            TilePoint::new(50, 50),
            TilePoint::new(0, 50),
            TilePoint::new(0, 0),
        ];
        let hole = vec![
            TilePoint::new(10, 10),
            TilePoint::new(26, 10),
            TilePoint::new(26, 26),
            TilePoint::new(10, 26),
            TilePoint::new(10, 10),
        ];
        let group = TileRingGroup {
            exterior: shell,
            holes: vec![hole],
        };
        let kept = filter_tile_polygon_areas(vec![group.clone()], 256., 1.);
        assert_eq!(kept.len(), 1);
        assert!(kept[0].holes.is_empty());
        assert_eq!(
            filter_tile_polygon_areas(vec![group], 256., 0.)[0]
                .holes
                .len(),
            1
        );
    }
    #[test]
    fn layer_keys_preserve_frame_bytes_and_arbitrary_names() {
        let mut names = super::super::LAYERS
            .iter()
            .map(|s| s.to_string())
            .collect::<Vec<_>>();
        names.extend([
            String::new(),
            "custom-é".into(),
            "z".repeat(65),
            "水".repeat(42),
        ]);
        for name in names {
            let mut r = record(17);
            r.layer = name.clone().into();
            r.tile_id = 42;
            r.sort_key = -9;
            let encoded = EncodedRecord::encode(
                &r,
                Seq {
                    source: 4,
                    feature: 5,
                    record: 6,
                },
            )
            .unwrap();
            let known = LAYER_NAMES.iter().position(|n| *n == name);
            assert_eq!(
                known.is_some(),
                super::super::LAYERS.contains(&name.as_str())
            );
            // The frame, whose size the budget counts, spelled out.
            let payload_len = encoded.payload.len();
            assert!(payload_len < 128);
            let mut header = vec![payload_len as u8, 42];
            match known {
                Some(id) => header.push(id as u8),
                None => {
                    header.extend([LAYER_VERBATIM, name.len() as u8]);
                    header.extend_from_slice(name.as_bytes());
                }
            }
            header.extend([17, 17, 4, 5, 6]);
            assert_eq!(encoded.frame_bytes(), 2 + header.len() + payload_len);
            let mut replayed = read_run(&run_bytes(&[&encoded])).unwrap();
            assert_eq!(replayed.len(), 1);
            let replayed = replayed.pop().unwrap();
            assert_eq!(replayed.key, encoded.key);
            assert_eq!(
                matches!(replayed.key.layer, Layer::Known(_)),
                known.is_some()
            );
            let decoded = replayed.decode().unwrap();
            assert_eq!(decoded.layer, name);
            assert_eq!(
                (decoded.tile_id, decoded.sort_key, decoded.id),
                (42, -9, 17)
            );
        }
        let mut long = record(17);
        long.layer = "z".repeat(MAX_LAYER_BYTES + 1).into();
        assert!(EncodedRecord::encode(&long, seq(0)).is_err());
    }
    #[test]
    fn layer_ids_order_like_layer_names() {
        let mut names = super::super::LAYERS.to_vec();
        names.extend([
            "", "a", "poi", "poi2", "pois", "water_", "waterx", "zzz", "Water",
        ]);
        for a in &names {
            for b in &names {
                let key = |name: &str| {
                    let mut r = record(17);
                    r.layer = name.to_owned().into();
                    SortKey::of(&r, seq(0))
                };
                assert_eq!(key(a).cmp(&key(b)), a.cmp(b), "{a} vs {b}");
            }
        }
    }
    #[test]
    fn nearby_connected_skip_matches_reference_geometry_and_reduces_distance_tests() {
        let square = |x: f64, y: f64| {
            MultiPolygon(vec![Polygon::new(
                LineString::from(vec![
                    (x, y),
                    (x + 10., y),
                    (x + 10., y + 10.),
                    (x, y + 10.),
                    (x, y),
                ]),
                vec![],
            )])
        };
        let cases = vec![
            vec![
                square(0., 0.),
                square(1., 1.),
                square(2., 2.),
                square(3., 3.),
            ],
            vec![
                square(0., 0.),
                square(10.5, 0.),
                square(21., 0.),
                square(31.5, 0.),
            ],
            vec![
                square(0., 0.),
                square(1., 1.),
                square(100., 100.),
                square(101., 101.),
            ],
            vec![MultiPolygon(vec![]), square(0., 0.)],
        ];
        let mut reduced = false;
        for geometries in cases {
            let mut old_count = 0;
            let mut new_count = 0;
            let old = merge_nearby_impl::<false>(geometries.clone(), 1., &mut old_count);
            let new = merge_nearby_impl::<true>(geometries, 1., &mut new_count);
            assert_eq!(new, old);
            assert!(new_count <= old_count);
            reduced |= new_count < old_count;
        }
        assert!(
            reduced,
            "clique must avoid already-connected polygon distances"
        );
    }

    #[test]
    fn minimum_length_zero_and_positive_match_previous_filter() {
        let paths = vec![
            vec![],
            vec![TilePoint::new(0, 0)],
            vec![TilePoint::new(0, 0), TilePoint::new(3, 4)],
            vec![
                TilePoint::new(i32::MIN, i32::MIN),
                TilePoint::new(i32::MAX, i32::MAX),
            ],
        ];
        for threshold in [0., 0.1, 5., 5.1] {
            let mut expected = paths.clone();
            expected.retain(|p| path_length(p) >= threshold);
            let mut actual = paths.clone();
            simplify_and_filter_lines(&mut actual, 0., threshold);
            assert_eq!(actual, expected);
        }
        assert_eq!(
            line_merge_policy("transportation", 8, &AttrMap::new())
                .unwrap()
                .0,
            0.
        );
        assert!(
            line_merge_policy("transportation", 7, &AttrMap::new())
                .unwrap()
                .0
                > 0.
        );
    }

    #[test]
    fn worker_decode_preserves_record_order_and_rejects_corrupt_records() {
        let make_job = |records: Vec<EncodedRecord>| TileJob {
            id: 0,
            records,
            old: None,
        };
        let decoded = make_job(vec![
            EncodedRecord::encode(&record(9), seq(0)).unwrap(),
            EncodedRecord::encode(&record(2), seq(0)).unwrap(),
        ])
        .decode()
        .unwrap();
        assert_eq!(decoded.iter().map(|r| r.id).collect::<Vec<_>>(), vec![9, 2]);
        let mut corrupt = EncodedRecord::encode(&record(9), seq(0)).unwrap();
        corrupt.payload.truncate(3);
        assert!(make_job(vec![corrupt]).decode().is_err());
        let mut wrong_tile = EncodedRecord::encode(&record(9), seq(0)).unwrap();
        wrong_tile.key.tile_id = 1;
        assert!(
            make_job(vec![wrong_tile])
                .decode()
                .unwrap_err()
                .contains("different tile")
        );
    }

    #[test]
    fn combine_keeps_integer_ring_order_and_holes() {
        let mut a = polygon_record("building", 14, 12, 0);
        let b = polygon_record("building", 14, 22, 100);
        let TileGeometry::Polygon(groups) = &mut a.geometry else {
            unreachable!()
        };
        groups[0].holes.push(vec![
            TilePoint::new(4, 4),
            TilePoint::new(4, 8),
            TilePoint::new(8, 8),
            TilePoint::new(8, 4),
            TilePoint::new(4, 4),
        ]);
        let TileGeometry::Polygon(mut expected) = a.geometry.clone() else {
            unreachable!()
        };
        let TileGeometry::Polygon(second) = b.geometry.clone() else {
            unreachable!()
        };
        expected.extend(second);
        let result = merge_geometries(vec![a, b]).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].geometry, TileGeometry::Polygon(expected));
    }

    #[test]
    fn metadata_is_independent_of_worker_split_and_merge_order() {
        // Reference: fold records one at a time in insertion order.
        type Fields = BTreeMap<String, BTreeMap<String, String>>;
        fn serial(records: &[Record]) -> (Fields, BTreeMap<String, (u8, u8)>) {
            let mut fields: BTreeMap<String, BTreeMap<String, String>> = BTreeMap::new();
            let mut zooms: BTreeMap<String, (u8, u8)> = BTreeMap::new();
            for record in records {
                let z = zooms
                    .entry(record.layer.to_string())
                    .or_insert((record.zoom, record.zoom));
                z.0 = z.0.min(record.zoom);
                z.1 = z.1.max(record.zoom);
                let fields = fields.entry(record.layer.to_string()).or_default();
                for (key, value) in &record.attrs {
                    if !key.starts_with('_') {
                        let kind = match value {
                            AttrValue::String(_) => "String",
                            AttrValue::Bool(_) => "Boolean",
                            _ => "Number",
                        };
                        fields
                            .entry(key.to_string())
                            .and_modify(|old| {
                                if old != kind {
                                    *old = "String".into();
                                }
                            })
                            .or_insert_with(|| kind.into());
                    }
                }
                if record.rank {
                    fields
                        .entry("rank".into())
                        .or_insert_with(|| "Number".into());
                }
            }
            (fields, zooms)
        }
        let temp = tempfile::tempdir().unwrap();
        let mut combined = Store::new(temp.path()).unwrap();
        for a in 0..8 {
            for b in 0..8 {
                for c in 0..8 {
                    let records = [a, b, c]
                        .into_iter()
                        .enumerate()
                        .map(|(i, variant)| {
                            let mut r = record(i as u64);
                            r.zoom = [14, 3, 9][i];
                            r.rank = variant & 4 != 0;
                            r.attrs.clear();
                            match variant & 3 {
                                1 => {
                                    r.attrs.insert("rank".into(), AttrValue::Bool(true));
                                }
                                2 => {
                                    r.attrs.insert("rank".into(), AttrValue::Int(2));
                                }
                                3 => {
                                    r.attrs
                                        .insert("rank".into(), AttrValue::String("text".into()));
                                }
                                _ => {}
                            }
                            r.attrs.insert("_private".into(), AttrValue::Bool(true));
                            r
                        })
                        .collect::<Vec<_>>();
                    let (fields, zooms) = serial(&records);
                    // Every assignment of the three records to two workers,
                    // merged in both orders.
                    for assignment in 0..8u32 {
                        for reversed in [false, true] {
                            let mut parts =
                                [MetadataSummary::default(), MetadataSummary::default()];
                            for (i, r) in records.iter().enumerate() {
                                parts[(assignment >> i) as usize & 1].observe(r, seq(i as u64));
                            }
                            let [mut x, mut y] = parts;
                            if reversed {
                                std::mem::swap(&mut x, &mut y);
                            }
                            x.merge(y);
                            combined.fields.clear();
                            combined.zooms.clear();
                            x.apply(&mut combined);
                            assert_eq!(
                                fields, combined.fields,
                                "variants {a},{b},{c} assignment {assignment} reversed {reversed}"
                            );
                            assert_eq!(zooms, combined.zooms);
                        }
                    }
                }
            }
        }
    }
    #[test]
    fn opaque_key_matches_record_comparator() {
        let mut records = vec![];
        for tile in [u64::MAX, 0, 1] {
            for layer in ["water", "", "a", "é"] {
                for sort in [i64::MIN, 0, i64::MAX] {
                    for id in [u64::MAX, 0, 3] {
                        let mut r = record(id);
                        r.tile_id = tile;
                        r.layer = layer.into();
                        r.sort_key = sort;
                        records.push(r);
                    }
                }
            }
        }
        for a in &records {
            for b in &records {
                assert_eq!(
                    order(a, b),
                    SortKey::of(a, seq(0)).cmp(&SortKey::of(b, seq(0)))
                );
            }
        }
    }
    fn run_bytes(records: &[&EncodedRecord]) -> Vec<u8> {
        let mut writer = RunWriter::new(vec![]).unwrap();
        for record in records {
            writer.write(record).unwrap();
        }
        writer.into_inner()
    }
    fn read_run(bytes: &[u8]) -> Result<Vec<EncodedRecord>, String> {
        let mut reader = RunReader::new(bytes);
        let mut records = vec![];
        while let Some(record) = reader.next()? {
            records.push(record);
        }
        Ok(records)
    }
    #[test]
    fn runs_reject_truncation_and_bad_payloads() {
        let encoded = EncodedRecord::encode(&record(1), seq(0)).unwrap();
        let bytes = run_bytes(&[&encoded]);
        assert_eq!(read_run(&bytes).unwrap()[0].payload, encoded.payload);
        // A run cut after its magic, between blocks, reads as empty.
        for end in (0..bytes.len()).filter(|&end| end != 4) {
            assert!(read_run(&bytes[..end]).is_err(), "prefix {end}");
        }
        let mut writer = RunWriter::new(vec![]).unwrap();
        let mut bad = EncodedRecord::encode(&record(1), seq(0)).unwrap();
        bad.payload[0] ^= 0xff;
        bad.payload.truncate(3);
        assert!(writer.write(&bad).is_err());
        bad.payload.clear();
        assert!(writer.write(&bad).is_err());
        writer.write(&encoded).unwrap();
        assert_eq!(
            read_run(&writer.into_inner()).unwrap()[0].payload,
            encoded.payload
        );
        let mut trailing = EncodedRecord::encode(&record(1), seq(0)).unwrap();
        trailing.payload.push(0);
        assert!(trailing.decode().is_err());
        let mut malformed = EncodedRecord::encode(&record(1), seq(0)).unwrap();
        malformed.payload.truncate(3);
        assert!(malformed.decode().is_err());
    }
    #[test]
    fn sorter_runs_hold_the_records_in_sort_key_order() {
        let temp = tempfile::tempdir().unwrap();
        let store = Store::new(temp.path()).unwrap();
        // Table and named layers, equal keys apart from the sequence, and a
        // budget small enough to spill several runs.
        let layers = ["poi", "zzz_custom", "building", "aaa_custom", "water"];
        let mut expected = vec![];
        let mut sorter = store.sorter(0, &SortBudget::fixed(600));
        for i in 0..200u64 {
            let mut r = record(i % 3);
            r.tile_id = (i * 7) % 4;
            r.layer = layers[(i as usize * 3) % layers.len()].into();
            r.sort_key = (i % 5) as i64 - 2;
            let seq = Seq {
                source: (i * 13) % 17,
                feature: (i % 2) as u32,
                record: i as u32,
            };
            let encoded = EncodedRecord::encode(&r, seq).unwrap();
            expected.push((encoded.key.clone(), encoded.payload.clone()));
            sorter.add(encoded).unwrap();
        }
        let runs = sorter.finish().unwrap();
        assert!(runs.len() > 1, "{} runs", runs.len());
        let mut found = vec![];
        for run in &runs {
            let mut reader = RunReader::open(run).unwrap();
            let mut records = vec![];
            while let Some(record) = reader.next().unwrap() {
                records.push((record.key, record.payload));
            }
            assert!(
                records.is_sorted_by(|a, b| a.0 <= b.0),
                "a run out of order"
            );
            found.extend(records);
        }
        found.sort_by(|a, b| a.0.cmp(&b.0));
        expected.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(found, expected);
    }

    #[test]
    fn merge_plan_merges_only_the_smallest_runs_it_needs() {
        let f = MERGE_FAN_IN;
        assert!(merge_plan(&vec![1; f]).is_empty());
        // One run too many: the two smallest become one.
        let mut sizes = (0..=f as u64).map(|run| 1000 - run).collect::<Vec<_>>();
        assert_eq!(merge_plan(&sizes), vec![vec![f, f - 1]]);
        // Many small early runs and a few large ones: only small ones merge.
        sizes = [vec![8; 3 * f], vec![1 << 20; 40]].concat();
        let plan = merge_plan(&sizes);
        assert!(plan.iter().flatten().all(|&run| sizes[run] == 8));
        let left = sizes.len() - plan.iter().map(Vec::len).sum::<usize>() + plan.len();
        assert_eq!(left, f);
        assert!(plan.iter().all(|group| (2..=f).contains(&group.len())));
        // More than one pass can reach: every run, in groups of at most f.
        sizes = vec![1; f * f + 1];
        let plan = merge_plan(&sizes);
        assert_eq!(plan.len(), f + 1);
        assert_eq!(plan.iter().map(Vec::len).sum::<usize>(), sizes.len());
        assert!(plan.iter().all(|group| group.len() <= f));
        let mut runs = plan.concat();
        runs.sort_unstable();
        assert!(runs.iter().copied().eq(0..sizes.len()));
    }

    #[test]
    fn sort_budget_grows_after_many_runs_up_to_its_memory() {
        let entry = size_of::<SortEntry>() as u64;
        let budget = SortBudget::new(100, Some(4 * 10_000), 4);
        // Runs below `GROW_AFTER_RUNS` keep the floor.
        for run in 1..GROW_AFTER_RUNS as u64 {
            assert_eq!(budget.after_spill(100, 100, run * 100, run), 100);
        }
        // Then a run is as large as the sorter's earlier ones together.
        assert_eq!(budget.after_spill(100, 100, 300, 3), 300);
        assert_eq!(budget.after_spill(300, 300, 600, 6), 600);
        // Up to the worker's 10,000 bytes, less its entries and their spare
        // capacity: records of 100 frame bytes leave 100 / (100 + 2 * 32).
        let ceiling = (10_000 * 100 / (100 + 2 * entry)) as usize;
        assert_eq!(budget.after_spill(600, 600, 100 << 14, 1 << 14), ceiling);
        // Never below the floor, however little memory there is.
        let starved = SortBudget::new(100, Some(10), 4);
        starved.runs.store(GROW_AFTER_RUNS, AtomicOrdering::Relaxed);
        assert_eq!(starved.after_spill(100, 100, 1 << 20, 1 << 14), 100);
        // Unknown memory keeps the floor too.
        let unknown = SortBudget::new(100, None, 4);
        unknown.runs.store(GROW_AFTER_RUNS, AtomicOrdering::Relaxed);
        assert_eq!(unknown.after_spill(100, 100, 1 << 20, 1 << 14), 100);
    }

    #[test]
    fn sort_budget_with_an_estimate_grows_only_as_far_as_the_target_needs() {
        let plenty = Some(1 << 40);
        // 100,000 frame bytes expected: runs below `PLAN_AFTER_RUNS` keep
        // the floor.
        let budget = SortBudget::new(100, plenty, 4).expecting(Some(100_000));
        for run in 1..PLAN_AFTER_RUNS as u64 {
            assert_eq!(budget.after_spill(100, 100, 10_000, run), 100);
        }
        // Then the bytes still expected, over the runs left to `TARGET_RUNS`:
        // 98,400 bytes over 80 runs. Not what the sorter spilled (10,000).
        let total = PLAN_AFTER_RUNS as u64 * 100;
        let slots = (TARGET_RUNS - PLAN_AFTER_RUNS) as u64;
        let needed = (100_000 - total).div_ceil(slots) as usize;
        assert_eq!(budget.after_spill(100, 100, 10_000, 100), needed);
        // A buffer never shrinks, and grows no further while it is enough.
        assert_eq!(budget.after_spill(5_000, 100, 10_000, 100), 5_000);

        // A compile spilling far less than expected (a small area of a large
        // extract) grows at most by doubling, as without an estimate.
        let over = SortBudget::new(100, plenty, 4).expecting(Some(1 << 30));
        over.runs.store(PLAN_AFTER_RUNS, AtomicOrdering::Relaxed);
        assert_eq!(over.after_spill(100, 100, 200, 2), 200);
        assert_eq!(over.after_spill(200, 200, 400, 4), 400);

        // One that spills more than expected has a quarter more to come:
        // 1,700 bytes spilled of an estimate of 1,000 leaves 425.
        let under = SortBudget::new(1, plenty, 4).expecting(Some(1_000));
        under.runs.store(PLAN_AFTER_RUNS, AtomicOrdering::Relaxed);
        under.spilled.store(1_600, AtomicOrdering::Relaxed);
        let slots = (TARGET_RUNS - PLAN_AFTER_RUNS - 1) as u64;
        assert_eq!(
            under.after_spill(1, 100, 1 << 20, 100),
            425u64.div_ceil(slots) as usize
        );
        // Past `TARGET_RUNS` the buffers double, up to the memory share.
        let late = SortBudget::new(100, Some(4 * 10_000), 4).expecting(Some(1_000));
        late.runs.store(TARGET_RUNS, AtomicOrdering::Relaxed);
        assert_eq!(late.after_spill(100, 100, 300, 1), 300);
    }

    #[test]
    fn opaque_merge_orders_equal_keys_by_sequence_across_multiple_passes() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::new(temp.path()).unwrap();
        // Equal sort keys; sequences are interleaved across runs and workers.
        for run in 0..20u64 {
            let mut sorter = store.sorter(run as usize, &SortBudget::fixed(usize::MAX));
            for offset in 0..3u64 {
                let sequence = offset * 20 + run;
                let mut r = record(7);
                r.attrs
                    .insert("sequence".into(), AttrValue::Int(sequence as i64));
                sorter
                    .add(EncodedRecord::encode(&r, seq(sequence)).unwrap())
                    .unwrap();
            }
            store.add_runs(sorter.finish().unwrap());
        }
        let mut intermediate = vec![];
        for (index, runs) in store.runs.chunks(3).enumerate() {
            let path = temp.path().join(format!("merge-{index}"));
            let mut writer = RunWriter::create(&path).unwrap();
            merge(runs, |r| writer.write(&r)).unwrap();
            writer.finish().unwrap();
            intermediate.push(path);
        }
        let mut values = vec![];
        merge(&intermediate, |r| {
            let r = r.decode()?;
            values.push(r.attrs["sequence"].clone());
            Ok(())
        })
        .unwrap();
        assert_eq!(values, (0..60).map(AttrValue::Int).collect::<Vec<_>>());
    }
    /// Runs whose records interleave across tiles and runs, each run many
    /// blocks long.
    fn interleaved_runs(temp: &Path) -> Store {
        let mut store = Store::new(temp).unwrap();
        for worker in 0..6u64 {
            let mut sorter = store.sorter(worker as usize, &SortBudget::fixed(400 * 300));
            for i in 0..1500u64 {
                let mut r = record(i);
                r.tile_id = (i * i * 7 + worker * 13) % 9000 / 30;
                r.attrs.insert(
                    "padding".into(),
                    AttrValue::String("x".repeat((i % 97) as usize * 3)),
                );
                sorter
                    .add(EncodedRecord::encode(&r, seq(worker * 10_000 + i)).unwrap())
                    .unwrap();
            }
            store.add_runs(sorter.finish().unwrap());
        }
        store
    }

    #[test]
    fn prefetched_merge_gives_the_records_of_one_merge() {
        let temp = tempfile::tempdir().unwrap();
        let store = interleaved_runs(temp.path());
        assert!(store.runs.len() > 6, "{} runs", store.runs.len());
        let mut expected = vec![];
        merge(&store.runs, |r| {
            expected.push((r.key, r.payload));
            Ok(())
        })
        .unwrap();
        assert_eq!(expected.len(), 6 * 1500);
        for decoders in [1, 2, 3, 16] {
            let mut found = vec![];
            merge_prefetched(&store.runs, decoders, &Control::detached(), |r| {
                found.push((r.key, r.payload));
                Ok(())
            })
            .unwrap();
            assert!(found == expected, "{decoders} decoders");
        }
        merge_prefetched(&[], 4, &Control::detached(), |_| panic!("no records")).unwrap();
    }

    #[test]
    fn prefetched_merge_stops_on_the_first_error() {
        let temp = tempfile::tempdir().unwrap();
        let store = interleaved_runs(temp.path());
        // The consumer failing while decoders still run.
        let mut emitted = 0;
        let result = merge_prefetched(&store.runs, 4, &Control::detached(), |_| {
            emitted += 1;
            if emitted == 100 {
                Err("writer failed".into())
            } else {
                Ok(())
            }
        });
        assert_eq!(result, Err("writer failed".into()));
        assert_eq!(emitted, 100);
        // A run damaged past its first block fails the merge where the merge
        // reaches the damage, after every record before it.
        let damaged = temp.path().join("damaged");
        let mut bytes = fs::read(&store.runs[1]).unwrap();
        let cut = bytes.len() * 2 / 3;
        bytes.truncate(cut);
        fs::write(&damaged, &bytes).unwrap();
        let mut runs = store.runs.clone();
        runs[1] = damaged;
        let mut serial = 0;
        let serial_error = merge(&runs, |_| {
            serial += 1;
            Ok(())
        })
        .unwrap_err();
        let mut prefetched = 0;
        let prefetched_error = merge_prefetched(&runs, 3, &Control::detached(), |_| {
            prefetched += 1;
            Ok(())
        })
        .unwrap_err();
        assert_eq!(prefetched_error, serial_error);
        assert!(prefetched > 0);
        assert_eq!(prefetched, serial);
        // A run that cannot be opened.
        let mut runs = store.runs.clone();
        runs[2] = temp.path().join("missing");
        assert!(
            merge_prefetched(&runs, 2, &Control::detached(), |_| Ok(()))
                .unwrap_err()
                .contains("missing")
        );
    }

    /// A test job: sleep `millis`, then succeed or fail with `error`
    /// (`"panic"` panics). The job's index is its result.
    type TestJob = (usize, u64, Option<&'static str>);
    fn run_job(_: &mut (), (index, millis, error): TestJob) -> Result<usize, String> {
        std::thread::sleep(std::time::Duration::from_millis(millis));
        match error {
            Some("panic") => panic!("injected"),
            Some(error) => Err(error.to_string()),
            None => Ok(index),
        }
    }
    /// Sends `jobs` (with their input bytes) through `encode_ordered`, then
    /// `producer_error`. Returns the written indices and the outcome.
    fn run_encoder(
        threads: usize,
        window: usize,
        byte_budget: usize,
        jobs: &[(u64, usize, Option<&'static str>)],
        producer_error: Option<&str>,
    ) -> (Vec<usize>, Result<(), String>) {
        let mut written = vec![];
        let result = encode_ordered(
            threads,
            window,
            byte_budget,
            || (),
            run_job,
            |index| {
                written.push(index);
                Ok(())
            },
            |stream| {
                for (index, &(millis, bytes, error)) in jobs.iter().enumerate() {
                    stream.send((index, millis, error), bytes)?;
                }
                producer_error.map_or(Ok(()), |error| Err(error.to_string()))
            },
        );
        (written, result)
    }
    #[test]
    fn encoder_writes_in_order_when_workers_finish_out_of_order() {
        // The slow first job finishes last; later jobs are not held back by it.
        let jobs = [(150, 1, None), (1, 1, None), (30, 1, None), (1, 1, None)];
        for threads in [1, 3] {
            let (written, result) = run_encoder(threads, 8, usize::MAX, &jobs, None);
            result.unwrap();
            assert_eq!(written, vec![0, 1, 2, 3]);
        }
    }
    #[test]
    fn encoder_dispatches_past_a_straggler_up_to_the_window() {
        use std::sync::atomic::AtomicUsize;
        let started = AtomicUsize::new(0);
        let started_when_straggler_ends = AtomicUsize::new(0);
        encode_ordered(
            4,
            3,
            usize::MAX,
            || (),
            |_, index: usize| {
                started.fetch_add(1, AtomicOrdering::SeqCst);
                if index == 0 {
                    std::thread::sleep(std::time::Duration::from_millis(300));
                    started_when_straggler_ends
                        .store(started.load(AtomicOrdering::SeqCst), AtomicOrdering::SeqCst);
                }
                Ok(())
            },
            |_| Ok(()),
            |stream| (0..8).try_for_each(|index| stream.send(index, 1)),
        )
        .unwrap();
        // Jobs 1 and 2 ran beside job 0; job 3 had to wait for job 0's write.
        assert_eq!(started_when_straggler_ends.load(AtomicOrdering::SeqCst), 3);
        assert_eq!(started.load(AtomicOrdering::SeqCst), 8);
    }
    #[test]
    fn encoder_runs_an_oversized_job_alone_but_overlaps_small_ones() {
        let events = Mutex::new(vec![]);
        let run = |first_bytes: usize| {
            events.lock().unwrap().clear();
            encode_ordered(
                2,
                8,
                10,
                || (),
                |_, index: usize| {
                    events.lock().unwrap().push(format!("start {index}"));
                    if index == 0 {
                        std::thread::sleep(std::time::Duration::from_millis(200));
                    }
                    events.lock().unwrap().push(format!("end {index}"));
                    Ok(())
                },
                |_| Ok(()),
                |stream| {
                    stream.send(0, first_bytes)?;
                    stream.send(1, 1)
                },
            )
            .unwrap();
            events.lock().unwrap().clone()
        };
        assert_eq!(run(11), ["start 0", "end 0", "start 1", "end 1"]);
        // Both jobs start on their own workers in either order; job 1 runs
        // entirely while job 0 is still encoding.
        let events = run(1);
        let at = |event: &str| events.iter().position(|e| e == event).unwrap();
        assert!(
            at("start 0") < at("end 1") && at("end 1") < at("end 0"),
            "{events:?}"
        );
    }
    #[test]
    fn encoder_returns_the_first_error_in_job_order_without_hanging() {
        // Job 2 fails before slow job 0 ends, but job 1 is earlier in order.
        let jobs = [
            (100, 1, None),
            (1, 1, Some("failure 1")),
            (1, 1, Some("failure 2")),
            (1, 1, None),
        ];
        let (written, result) = run_encoder(3, 8, usize::MAX, &jobs, None);
        assert_eq!(result.unwrap_err(), "failure 1");
        assert_eq!(written, vec![0]);
        // A failure with more jobs than the window still ends the run.
        let mut jobs = vec![(1, 1, None); 40];
        jobs[5].2 = Some("failure 5");
        let (written, result) = run_encoder(2, 2, usize::MAX, &jobs, Some("producer"));
        assert_eq!(result.unwrap_err(), "failure 5");
        assert_eq!(written, vec![0, 1, 2, 3, 4]);
    }
    #[test]
    fn encoder_panic_fails_the_job_instead_of_hanging() {
        let mut jobs = vec![(1, 1, None); 20];
        jobs[1].2 = Some("panic");
        for threads in [1, 2] {
            let (written, result) = run_encoder(threads, 2, usize::MAX, &jobs, None);
            assert!(result.unwrap_err().contains("panicked"));
            assert_eq!(written, vec![0]);
        }
    }
    #[test]
    fn encoder_writes_queued_jobs_before_a_producer_error() {
        let jobs = [(20, 1, None), (1, 1, None), (1, 1, None)];
        let (written, result) = run_encoder(3, 8, usize::MAX, &jobs, Some("producer"));
        assert_eq!(result.unwrap_err(), "producer");
        assert_eq!(written, vec![0, 1, 2]);
        // A job error precedes the producer's.
        let jobs = [(1, 1, None), (1, 1, Some("failure 1"))];
        let (written, result) = run_encoder(3, 8, usize::MAX, &jobs, Some("producer"));
        assert_eq!(result.unwrap_err(), "failure 1");
        assert_eq!(written, vec![0]);
    }
    #[test]
    fn scratch_directory_is_claimed_by_one_compile() {
        let temp = tempfile::tempdir().unwrap();
        // A sort directory a killed compile left is replaced.
        let stale = temp.path().join("omt-tile-sort");
        fs::create_dir(&stale).unwrap();
        fs::write(stale.join("run-w000-000000.bin"), b"stale").unwrap();
        let store = Store::new(temp.path()).unwrap();
        assert_eq!(fs::read_dir(&stale).unwrap().count(), 0);
        // A second compile is refused while the first holds it.
        let refused = Store::new(temp.path()).err().unwrap();
        assert!(refused.contains("in use by another compile"), "{refused}");
        assert!(stale.exists());
        drop(store);
        assert!(!stale.exists());
        drop(Store::new(temp.path()).unwrap());
    }
    #[test]
    fn serial_and_parallel_archives_are_identical() {
        let temp = tempfile::tempdir().unwrap();
        let bounds = CompileBounds::new(51., 49., 21., 19.);
        let mut outputs = vec![];
        for threads in [1, 3] {
            let workspace = temp.path().join(format!("work-{threads}"));
            std::fs::create_dir(&workspace).unwrap();
            let mut store = Store::new(&workspace).unwrap();
            let mut records = vec![];
            for tile in (0..21).rev() {
                for id in (0..4).rev() {
                    let mut r = record(id);
                    r.tile_id = tile;
                    r.grid_limit = 4;
                    r.attrs
                        .insert("name".into(), AttrValue::String(format!("name-{id}")));
                    records.push(r);
                }
            }
            add_run(&mut store, 0, 0, records);
            let output = temp.path().join(format!("out-{threads}.pmtiles"));
            let archive = File::create(&output).unwrap();
            assert_eq!(
                store
                    .finish(
                        &archive,
                        &ArchiveHeader::for_test(bounds, 0, 2),
                        threads,
                        &crate::control::Control::detached()
                    )
                    .unwrap()
                    .tiles(),
                21
            );
            outputs.push(std::fs::read(output).unwrap());
        }
        assert_eq!(outputs[0], outputs[1]);
    }
    #[test]
    fn university_point_source_is_filtered_through_zoom_thirteen() {
        use super::super::{
            render,
            types::{Feature, Source, SourceKind},
        };
        let geometry = geo_types::Geometry::Point(geo_types::Point::new(20.0, 50.0));
        let source = Source {
            id: 11,
            osm_id: 1,
            kind: SourceKind::Node,
            source_layer: "osm".into(),
            tags: BTreeMap::new(),
            geometry: geometry.clone(),
            relations: vec![],
        };
        let mut feature = Feature::new(&source, "poi", geometry.clone(), 10, 14);
        feature.min_size_source_geometry = Some(geometry);
        feature
            .attrs
            .insert("__university_min_size".into(), serde_json::json!(80));
        let mut zooms = std::collections::BTreeSet::new();
        render::emit_with_record_sink(
            feature,
            &CompileBounds::new(51.0, 49.0, 21.0, 19.0).into(),
            0,
            14,
            &mut |r| {
                zooms.insert(r.zoom);
                Ok(())
            },
        )
        .unwrap();
        // ZoomFunction.maxZoom(13, 80) includes z13 in the size override.
        assert_eq!(zooms, [14].into_iter().collect());
    }
    fn seq(source: u64) -> Seq {
        Seq {
            source,
            feature: 0,
            record: 0,
        }
    }
    /// Sorts records into one run owned by `store`; the position in `records`
    /// plus `first` is the insertion sequence.
    fn add_run(store: &mut Store, worker: usize, first: u64, records: Vec<Record>) {
        let mut sorter = store.sorter(worker, &SortBudget::fixed(usize::MAX));
        for (offset, r) in records.iter().enumerate() {
            sorter
                .add(EncodedRecord::encode(r, seq(first + offset as u64)).unwrap())
                .unwrap();
        }
        store.add_runs(sorter.finish().unwrap());
    }
    fn record(id: u64) -> Record {
        Record {
            tile_id: 0,
            zoom: 0,
            id,
            layer: "poi".into(),
            geometry: TileGeometry::Point(vec![TilePoint::new(20, 20)]),
            attrs: AttrMap::new(),
            sort_key: 0,
            grid_size: 64.,
            grid_cell: Some((0, 0)),
            grid_limit: 1,
            rank: true,
        }
    }
    fn polygon_record(layer: &str, zoom: u8, id: u64, x: i32) -> Record {
        let mut r = record(id);
        r.layer = layer.to_owned().into();
        r.zoom = zoom;
        r.grid_size = 0.;
        r.grid_cell = None;
        r.rank = false;
        r.geometry = TileGeometry::Polygon(vec![TileRingGroup {
            exterior: vec![
                TilePoint::new(x, 0),
                TilePoint::new(x + 64, 0),
                TilePoint::new(x + 64, 64),
                TilePoint::new(x, 64),
                TilePoint::new(x, 0),
            ],
            holes: vec![],
        }]);
        r
    }
    #[test]
    fn density_uses_source_cell_even_when_quantized_coordinates_coincide() {
        let mut a = record(19120068671);
        a.layer = "place".into();
        a.grid_limit = 1;
        a.geometry = TileGeometry::Point(vec![TilePoint::new(5995, 4096)]);
        a.grid_cell = Some((566, 347));
        let mut b = a.clone();
        b.id = 22812815521;
        b.grid_cell = Some((566, 348));
        let out = point_density(vec![a, b]);
        assert_eq!(out.len(), 2);
        assert!(out.iter().all(|r| r.attrs["rank"] == AttrValue::Int(11)));
    }
    #[test]
    fn previous_record_schema_is_rejected_before_payload_decode() {
        let mut bytes = run_bytes(&[&EncodedRecord::encode(&record(1), seq(0)).unwrap()]);
        bytes[3] = b'1';
        assert!(read_run(&bytes).is_err());
    }
    #[test]
    fn default_group_ranks_continue_when_density_grid_is_disabled() {
        for (layer, offset) in [("place", 10), ("mountain_peak", 0)] {
            let input = (1..=3)
                .map(|id| {
                    let mut r = record(id);
                    r.layer = layer.into();
                    r.grid_size = 0.;
                    r.grid_cell = None;
                    r.grid_limit = 0;
                    r
                })
                .collect();
            let out = point_density(input);
            assert_eq!(
                out.iter()
                    .map(|r| r.attrs["rank"].clone())
                    .collect::<Vec<_>>(),
                vec![
                    AttrValue::Int(1 + offset),
                    AttrValue::Int(2 + offset),
                    AttrValue::Int(3 + offset)
                ]
            );
        }
    }
    #[test]
    fn landcover_merges_only_source_policy_groups() {
        let feature = |zoom, count, id| {
            let mut r = polygon_record("landcover", zoom, id, 0);
            r.attrs
                .insert("subclass".into(), AttrValue::String("forest".into()));
            r.attrs.insert("_numpoints".into(), AttrValue::Int(count));
            r
        };
        for zoom in [10, 13, 14] {
            let out = merge_geometries(vec![
                feature(zoom, 100, 12),
                feature(zoom, 100, 22),
                feature(zoom, 301, 32),
            ])
            .unwrap();
            assert_eq!(out.len(), if zoom == 14 { 3 } else { 2 });
            assert!(out.iter().all(|r| !r.attrs.contains_key("_numpoints")));
        }
        let out = merge_geometries(vec![feature(8, 100, 12), feature(8, 301, 22)]).unwrap();
        assert_eq!(out.len(), 2);
    }
    #[test]
    fn nonresidential_landuse_is_not_merged_and_buildings_are_grouped() {
        let mut a = polygon_record("landuse", 14, 12, 0);
        a.attrs
            .insert("class".into(), AttrValue::String("industrial".into()));
        let mut b = a.clone();
        b.id = 22;
        assert_eq!(merge_geometries(vec![a, b]).unwrap().len(), 2);
        let out = merge_geometries(vec![
            polygon_record("building", 14, 12, 0),
            polygon_record("building", 14, 22, 100),
        ])
        .unwrap();
        assert_eq!(out.len(), 1);
        assert_eq!(out[0].id, 10);
        assert!(matches!(&out[0].geometry,TileGeometry::Polygon(p) if p.len()==2));
    }
    #[test]
    fn nearby_building_grouping_obeys_distance_not_double_buffer() {
        let close = merge_geometries(vec![
            polygon_record("building", 13, 12, 0),
            polygon_record("building", 13, 22, 70),
        ])
        .unwrap();
        assert!(matches!(&close[0].geometry,TileGeometry::Polygon(p) if p.len()==1));
        let far = merge_geometries(vec![
            polygon_record("building", 13, 12, 0),
            polygon_record("building", 13, 22, 76),
        ])
        .unwrap();
        assert!(matches!(&far[0].geometry,TileGeometry::Polygon(p) if p.len()==2));
    }
    #[test]
    fn typed_roundtrip_and_truncation() {
        let mut r = record(1);
        r.attrs
            .insert("class".into(), AttrValue::String("cafe".into()));
        let bytes = run_bytes(&[&EncodedRecord::encode(&r, seq(0)).unwrap()]);
        let got = read_run(&bytes).unwrap().pop().unwrap().decode().unwrap();
        assert_eq!(got.attrs, r.attrs);
        assert!(read_run(&bytes[..bytes.len() - 1]).is_err());
    }
    #[test]
    fn density_applies_order_and_adds_rank() {
        let result = point_density(vec![record(1), record(2)]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].attrs["rank"], AttrValue::Int(1));
    }
    #[test]
    fn housenumber_merge_id_uses_java_partition_iteration_order() {
        let house = |id, partition: &str| {
            let mut r = record(id);
            r.layer = "housenumber".into();
            r.zoom = 14;
            r.rank = false;
            r.grid_cell = None;
            r.grid_limit = 0;
            r.attrs
                .insert("housenumber".into(), AttrValue::String("7".into()));
            r.attrs.insert(
                "__housenumber_partition".into(),
                AttrValue::String(partition.into()),
            );
            r.attrs
                .insert("__housenumber_has_name".into(), AttrValue::Bool(false));
            r
        };
        // Java String hashes for Aa and BB collide. HashMap.computeIfAbsent
        // inserts BB at the bin head, so FeatureMerge uses its ID first.
        let records = point_density(vec![house(11, "Aa"), house(21, "BB")]);
        assert_eq!(records.iter().map(|r| r.id).collect::<Vec<_>>(), [21, 11]);
        let merged = merge_geometries(records).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, 20);
        assert_eq!(
            merged[0].attrs["housenumber"],
            AttrValue::String("7".into())
        );
    }
    #[test]
    fn external_runs_merge_in_tile_order() {
        let temp = tempfile::tempdir().unwrap();
        let mut store = Store::new(temp.path()).unwrap();
        let mut b = record(2);
        b.tile_id = 2;
        add_run(&mut store, 0, 0, vec![b]);
        add_run(&mut store, 1, 1, vec![record(1)]);
        let mut ids = vec![];
        merge(&store.runs, |r| {
            ids.push(r.key.tile_id);
            Ok(())
        })
        .unwrap();
        assert_eq!(ids, vec![0, 2]);
    }
    #[test]
    fn subgrid_branch_does_not_split_an_existing_long_label_path() {
        let p = TilePoint::new;
        let input = vec![
            vec![p(0, 0), p(20, 0)],
            vec![p(20, 0), p(40, 0)],
            vec![p(19, 0), p(19, 10)],
        ];
        let mut paths = join_paths(input);
        snap_line_paths(&mut paths, 1);
        simplify_and_filter_lines(&mut paths, 1.6, 15.);
        assert_eq!(paths, vec![vec![p(0, 0), p(10, 0), p(20, 0)]]);
    }

    #[test]
    fn waterway_rejoins_surviving_edges_after_precision_snap() {
        // At z9, both source edges exceed the minimum merged length. Their
        // pre-snap endpoints differ by one subgrid unit, but the pinned JTS
        // precision model places both at the same final coordinate. The
        // earlier native endpoint join cannot see that shared point.
        let p = TilePoint::new;
        let mut first = record(1);
        first.layer = "waterway".into();
        first.zoom = 9;
        first.rank = false;
        first.geometry = TileGeometry::Line {
            paths: vec![vec![p(0, 0), p(999, 10)]],
            scale_exp: 1,
        };
        let mut second = first.clone();
        second.id = 2;
        second.geometry = TileGeometry::Line {
            paths: vec![vec![p(1000, 10), p(2000, 0)]],
            scale_exp: 1,
        };
        let merged = merge_geometries(vec![first, second]).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].geometry,
            TileGeometry::Line {
                paths: vec![vec![p(0, 0), p(500, 5), p(1000, 0)]],
                scale_exp: 0,
            }
        );
    }

    #[test]
    fn waterway_short_snapped_prefix_survives_when_joined_length_passes() {
        // Pinned LoopLineMerger snaps before its final min-length removal.
        // Java oracle ShortWaterwayPrefixOracle produces (0,0),
        // (3.125,0.625), (34.375,0) pixels for these two edges at z9.
        let p = TilePoint::new;
        let mut first = record(1);
        first.layer = "waterway".into();
        first.zoom = 9;
        first.rank = false;
        first.geometry = TileGeometry::Line {
            paths: vec![vec![p(0, 0), p(99, 20)]],
            scale_exp: 1,
        };
        let mut second = first.clone();
        second.id = 2;
        second.geometry = TileGeometry::Line {
            paths: vec![vec![p(100, 20), p(1100, 0)]],
            scale_exp: 1,
        };
        let merged = merge_geometries(vec![first, second]).unwrap();
        assert_eq!(merged.len(), 1);
        assert_eq!(
            merged[0].geometry,
            TileGeometry::Line {
                paths: vec![vec![p(0, 0), p(50, 10), p(550, 0)]],
                scale_exp: 0,
            }
        );
    }

    #[test]
    fn merged_line_length_filter_uses_unsimplified_length() {
        let p = TilePoint::new;
        let mut paths = vec![vec![p(0, 0), p(10, 1), p(20, 0)]];
        simplify_and_filter_lines(&mut paths, 1.6, 20.05);
        assert_eq!(paths, vec![vec![p(0, 0), p(20, 0)]]);
        let mut short = vec![vec![p(0, 0), p(10, 1), p(20, 0)]];
        simplify_and_filter_lines(&mut short, 1.6, 20.2);
        assert!(short.is_empty());
    }

    #[test]
    fn merged_line_simplification_matches_kamieniec_java_oracle() {
        // Malopolskie way176240791, z14/9065/5555. The shared return point
        // anchors a loop; the pinned merger removes only the near-collinear
        // (4061,49) vertex after the renderer has quantized the line.
        let source = [
            (4160, -1),
            (4120, 20),
            (4075, 41),
            (4061, 49),
            (4049, 57),
            (4045, 65),
            (4045, 74),
            (4056, 88),
            (4074, 89),
            (4088, 78),
            (4101, 58),
            (4120, 20),
        ];
        let mut paths = vec![source.iter().map(|&(x, y)| TilePoint::new(x, y)).collect()];
        simplify_merged_lines(&mut paths, 1.);
        let expected: Vec<_> = source
            .iter()
            .filter(|&&p| p != (4061, 49))
            .map(|&(x, y)| TilePoint::new(x, y))
            .collect();
        assert_eq!(paths, vec![expected]);
    }

    #[test]
    fn merged_line_simplification_preserves_interior_junctions() {
        let p = TilePoint::new;
        let mut paths = vec![vec![p(0, 0), p(10, 1), p(20, 0)], vec![p(10, 1), p(10, 20)]];
        let expected = paths.clone();
        simplify_merged_lines(&mut paths, 1.6);
        assert_eq!(paths, expected);
        let mut no_branch = vec![expected[0].clone()];
        simplify_merged_lines(&mut no_branch, 1.6);
        assert_eq!(no_branch, vec![vec![p(0, 0), p(20, 0)]]);
    }

    #[test]
    fn merger_snaps_before_simplification_with_java_half_ties() {
        let p = TilePoint::new;
        let mut paths = vec![
            vec![p(-1, -1), p(0, 0), p(1, 1), p(8, 0)],
            vec![p(0, 0), p(-1, -1)],
        ];
        snap_line_paths(&mut paths, 1);
        assert_eq!(paths, vec![vec![p(0, 0), p(1, 1), p(4, 0)]]);
        simplify_merged_lines(&mut paths, 1.6);
        assert_eq!(paths, vec![vec![p(0, 0), p(4, 0)]]);
        assert_eq!(
            line_merge_policy("boundary", 8, &AttrMap::new()),
            Some((1. / 16., 4.))
        );
    }

    #[test]
    fn transportation_name_length_threshold_matches_pinned_profile() {
        let mut attrs = AttrMap::new();
        attrs.insert("name".into(), AttrValue::String("A😀".into()));
        assert_eq!(
            line_merge_policy("transportation_name", 12, &attrs),
            Some((18., 8.))
        );
        assert_eq!(
            line_merge_policy("transportation_name", 14, &attrs),
            Some((0., 8.))
        );
        attrs.clear();
        attrs.insert("ref".into(), AttrValue::String("DW7".into()));
        assert_eq!(
            line_merge_policy("transportation_name", 13, &attrs),
            Some((18., 8.))
        );
        attrs.clear();
        assert_eq!(
            line_merge_policy("transportation_name", 12, &attrs),
            Some((f64::MAX, 8.))
        );
    }

    #[test]
    fn joins_degree_two_but_preserves_branch() {
        let p = |x, y| TilePoint::new(x, y);
        let result = join_paths(vec![vec![p(0, 0), p(1, 0)], vec![p(1, 0), p(2, 0)]]);
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].len(), 3);
        let branch = join_paths(vec![
            vec![p(0, 0), p(1, 0)],
            vec![p(1, 0), p(2, 0)],
            vec![p(1, 0), p(1, 1)],
        ]);
        assert_eq!(branch.len(), 3);
    }

    #[test]
    #[ignore = "diagnostic: writes native buffer and snap stages for captured building inputs"]
    fn captured_building_float_merge_stages() {
        use crate::coordinate_groups::{WorldPoint, WorldRingGroup};
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-building-10954906650-merge.json"
        ))
        .unwrap();
        let records: Vec<Record> = serde_json::from_value(fixture["records"].clone()).unwrap();
        let geometries = records
            .iter()
            .map(|r| match &r.geometry {
                TileGeometry::Polygon(groups) => to_polygons(groups),
                _ => panic!("building fixture must contain polygons"),
            })
            .collect::<Vec<_>>();
        let buffer = |g: &MultiPolygon<f64>, amount: f64| {
            g.buffer_with_style(
                BufferStyle::new(amount).line_join(LineJoin::Miter(2. * (1_f64 / 5.).asin())),
            )
        };
        let expanded = geometries.iter().map(|g| buffer(g, 8.)).collect::<Vec<_>>();
        let unioned = unary_union(&expanded);
        let contracted = buffer(&unioned, -8.);
        let merged = merge_nearby(geometries, 8.);
        assert_eq!(merged.len(), 1);
        let (_, merged, _) = &merged[0];
        let buffered_zero = buffer(merged, 0.);
        let unioned_zero = unary_union(&merged.0);
        let convert = |ring: &LineString<f64>| {
            ring.0
                .iter()
                .map(|p| WorldPoint::new(p.x, p.y))
                .collect::<Vec<_>>()
        };
        let tile_groups = |polygons: &MultiPolygon<f64>| {
            polygons
                .0
                .iter()
                .map(|p| WorldRingGroup {
                    exterior: convert(p.exterior()),
                    holes: p.interiors().iter().map(&convert).collect(),
                })
                .collect()
        };
        let hot = super::super::render::snap_float_polygon_groups(tile_groups(merged));
        let hot_buffered_zero =
            super::super::render::snap_float_polygon_groups(tile_groups(&buffered_zero));
        let hot_unioned_zero =
            super::super::render::snap_float_polygon_groups(tile_groups(&unioned_zero));
        let output = serde_json::json!({
            "expanded": expanded,
            "unioned": unioned,
            "contracted": contracted,
            "merged": merged,
            "naive": from_polygons(merged.clone()),
            "hot_pixel": hot,
            "buffered_zero": buffered_zero,
            "unioned_zero": unioned_zero,
            "hot_buffered_zero": hot_buffered_zero,
            "hot_unioned_zero": hot_unioned_zero,
        });
        let path = std::env::var("OSM_TILE_COMPILER_BUILDING_STAGES").unwrap();
        std::fs::write(path, serde_json::to_vec_pretty(&output).unwrap()).unwrap();
    }
}

/// With the `benchmark-profiling` feature and `OTC_MERGE_DUMP=<path>`, every
/// dissolve group is appended to that file as one JSON line, for the offline
/// merge benchmark (`merge_replay` below).
#[cfg(feature = "benchmark-profiling")]
pub(super) mod merge_dump {
    use crate::tile_geometry::TileRingGroup;
    use std::io::Write;
    use std::sync::{Mutex, OnceLock};

    static FILE: OnceLock<Option<Mutex<std::io::BufWriter<std::fs::File>>>> = OnceLock::new();

    pub(in crate::omt) fn record(
        layer: &str,
        zoom: u8,
        tile_id: u64,
        distance: f64,
        members: &[Vec<TileRingGroup>],
    ) {
        let Some(file) = FILE.get_or_init(|| {
            std::env::var_os("OTC_MERGE_DUMP").map(|p| {
                Mutex::new(std::io::BufWriter::new(
                    std::fs::File::create(p).expect("merge dump file"),
                ))
            })
        }) else {
            return;
        };
        let geoms: Vec<Vec<Vec<Vec<[i32; 2]>>>> = members
            .iter()
            .map(|groups| {
                groups
                    .iter()
                    .map(|g| {
                        std::iter::once(&g.exterior)
                            .chain(g.holes.iter())
                            .map(|r| r.iter().map(|p| [p.x, p.y]).collect())
                            .collect()
                    })
                    .collect()
            })
            .collect();
        let line = serde_json::json!({
            "layer": layer, "zoom": zoom, "tile": tile_id, "distance": distance, "geoms": geoms
        });
        let mut f = file.lock().unwrap();
        writeln!(f, "{line}").unwrap();
        f.flush().unwrap();
    }
}

/// Offline replay of dissolve groups dumped with `OTC_MERGE_DUMP`:
/// `OTC_BENCH_GROUPS=<file> [OTC_BENCH_LAYER=building] cargo test --release
/// --lib merge_replay -- --ignored --nocapture`.
#[cfg(test)]
mod merge_replay {
    use super::*;
    use std::time::Instant;

    // The binary's allocator, so that replay timings match a compile's.
    #[cfg(feature = "mimalloc")]
    #[global_allocator]
    static GLOBAL: mimalloc::MiMalloc = mimalloc::MiMalloc;

    #[derive(Deserialize)]
    struct Group {
        layer: String,
        distance: f64,
        geoms: Vec<Vec<Vec<Vec<[i32; 2]>>>>,
    }

    fn to_geo(member: &[Vec<Vec<[i32; 2]>>]) -> MultiPolygon<f64> {
        let line = |r: &Vec<[i32; 2]>| {
            LineString(
                r.iter()
                    .map(|p| Coord {
                        x: f64::from(p[0]),
                        y: f64::from(p[1]),
                    })
                    .collect(),
            )
        };
        MultiPolygon(
            member
                .iter()
                .map(|rings| Polygon::new(line(&rings[0]), rings[1..].iter().map(line).collect()))
                .collect(),
        )
    }

    fn groups() -> Option<Vec<(f64, Vec<MultiPolygon<f64>>)>> {
        let path = std::env::var("OTC_BENCH_GROUPS").ok()?;
        let only = std::env::var("OTC_BENCH_LAYER").ok();
        let text = fs::read_to_string(path).unwrap();
        Some(
            text.lines()
                .map(|line| serde_json::from_str::<Group>(line).unwrap())
                .filter(|g| only.as_ref().is_none_or(|l| *l == g.layer))
                .map(|g| {
                    (
                        g.distance * 16.,
                        g.geoms.iter().map(|m| to_geo(m)).collect(),
                    )
                })
                .collect(),
        )
    }

    /// Every pair of members whose boxes come within the distance gets the
    /// same answer from `within_distance` as from geo's distance.
    #[test]
    #[ignore]
    fn merge_replay_proximity_matches_geo() {
        let Some(groups) = groups() else { return };
        let (mut pairs, mut differ) = (0usize, 0usize);
        let (mut geo_secs, mut ours_secs) = (0., 0.);
        for (distance, geoms) in &groups {
            let bounds: Vec<_> = geoms.iter().map(|g| g.bounding_rect()).collect();
            for i in 0..geoms.len() {
                for j in i + 1..geoms.len() {
                    let (Some(a), Some(b)) = (bounds[i], bounds[j]) else {
                        continue;
                    };
                    if b.min().x > a.max().x + distance
                        || a.min().x > b.max().x + distance
                        || b.min().y > a.max().y + distance
                        || b.max().y < a.min().y - distance
                    {
                        continue;
                    }
                    pairs += 1;
                    let started = Instant::now();
                    let reference = Euclidean.distance(&geoms[i], &geoms[j]) <= *distance;
                    geo_secs += started.elapsed().as_secs_f64();
                    let started = Instant::now();
                    let ours = within_distance(&geoms[i], &geoms[j], *distance);
                    ours_secs += started.elapsed().as_secs_f64();
                    if ours != reference {
                        differ += 1;
                        if differ <= 5 {
                            println!("differs (geo {reference}): {:?} {:?}", geoms[i], geoms[j]);
                        }
                    }
                }
            }
        }
        println!(
            "pairs {pairs}, differing {differ}, geo {geo_secs:.2}s, within_distance {ours_secs:.2}s"
        );
        assert_eq!(differ, 0);
    }

    thread_local! {
        /// Makes `merge_nearby` ask geo's distance, as before `within_distance`.
        pub(super) static GEO_PROXIMITY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }

    /// Times the full dissolve (proximity, buffer, union, unbuffer, snap)
    /// over the dumped groups on one thread, with each proximity test.
    #[test]
    #[ignore]
    fn merge_replay_timing() {
        let Some(groups) = groups() else { return };
        let variants: &[bool] = if std::env::var_os("OTC_BENCH_GEO").is_some() {
            &[true, false]
        } else {
            &[false]
        };
        for &geo in variants {
            GEO_PROXIMITY.with(|g| g.set(geo));
            replay_dissolve(
                &groups,
                if geo {
                    "geo proximity"
                } else {
                    "within_distance"
                },
            );
        }
        GEO_PROXIMITY.with(|g| g.set(false));
    }

    fn replay_dissolve(groups: &[(f64, Vec<MultiPolygon<f64>>)], label: &str) {
        use std::hash::{Hash, Hasher};
        // A digest of every merged cluster's coordinates, in order, so that two
        // builds can be compared bit for bit.
        let mut digest = std::collections::hash_map::DefaultHasher::new();
        let hash = |digest: &mut std::collections::hash_map::DefaultHasher,
                    g: &MultiPolygon<f64>| {
            for p in &g.0 {
                for ring in std::iter::once(p.exterior()).chain(p.interiors()) {
                    ring.0.len().hash(digest);
                    for c in &ring.0 {
                        (c.x.to_bits(), c.y.to_bits()).hash(digest);
                    }
                }
            }
        };
        let only_small = std::env::var_os("OTC_BENCH_SMALL").is_some();
        let mut total = 0.;
        for (distance, geoms) in groups.iter().cloned() {
            let geoms = if only_small {
                // Clusters of at most five members, each run on its own.
                let mut out = Vec::new();
                for (members, ..) in merge_nearby_collect(&geoms, distance) {
                    if (2..=5).contains(&members.len()) {
                        out.push(members.iter().map(|&i| geoms[i].clone()).collect());
                    }
                }
                out
            } else {
                vec![geoms]
            };
            for geoms in geoms {
                let started = Instant::now();
                let clusters = merge_nearby(geoms, distance);
                let mut snapped = Vec::new();
                for (members, merged, jts) in &clusters {
                    if members.len() > 1 && *jts {
                        snapped.push(super::super::jts_buffer::snap_and_fix(merged));
                    }
                }
                total += started.elapsed().as_secs_f64();
                for (members, merged, jts) in &clusters {
                    members.hash(&mut digest);
                    jts.hash(&mut digest);
                    hash(&mut digest, merged);
                }
                for s in snapped.iter().flatten() {
                    hash(&mut digest, s);
                }
            }
        }
        println!(
            "{label}: dissolve total {total:.2}s, digest {:016x}",
            digest.finish()
        );
    }

    /// Stage times of the positive-distance dissolve, by cluster size and by
    /// whether the area filter then drops the whole cluster, and how often
    /// cheap bounds on the output's area would have predicted the drop.
    #[test]
    #[ignore]
    fn merge_replay_stages() {
        use super::super::jts_buffer as jts;
        use geo::ConvexHull;
        let Some(groups) = groups() else { return };
        let min_area: f64 = std::env::var("OTC_BENCH_MIN_AREA")
            .ok()
            .map_or(4., |v| v.parse().unwrap())
            * 256.;
        #[derive(Default, Clone)]
        struct Row {
            clusters: usize,
            buffer: f64,
            union: f64,
            unbuffer: f64,
            snap: f64,
            hull: f64,
            hull_pred: usize,
            hull_pred_secs: f64,
            env_pred: usize,
            sum_pred: usize,
        }
        // [bucket][dropped]
        let mut rows = vec![[Row::default(), Row::default()]; 5];
        let mut proximity = 0.;
        let (mut singles, mut singles_dropped) = (0usize, 0usize);
        let mut worst_ratio = 0_f64;
        let mut violations = 0usize;
        let mut worst_excess = f64::MIN;
        let mut ratio_hist = [0usize; 12];
        let mut sound = [[(0usize, 0f64, 0usize, 0usize); 3]; 3];
        let mut sound_cost = 0.;
        const THRESHOLDS: [f64; 8] = [0.25, 0.375, 0.5, 0.625, 0.75, 0.875, 1.0, 1.25];
        let mut sweep = [[(0usize, 0f64, 0f64); 8]; 3];
        for (distance, geoms) in &groups {
            if *distance <= 0. {
                continue;
            }
            let started = Instant::now();
            let clusters = merge_nearby_collect(geoms, *distance);
            proximity += started.elapsed().as_secs_f64();
            for (members, ..) in clusters {
                if members.len() == 1 {
                    singles += 1;
                    let a = geoms[members[0]]
                        .0
                        .iter()
                        .map(|p| Polygon::new(p.exterior().clone(), vec![]).unsigned_area())
                        .fold(0., f64::max);
                    if a <= min_area {
                        singles_dropped += 1;
                    }
                    continue;
                }
                let cluster: Vec<MultiPolygon<f64>> =
                    members.iter().map(|&i| geoms[i].clone()).collect();
                let t0 = Instant::now();
                let expanded = cluster
                    .iter()
                    .map(|g| jts::buffer(g, *distance))
                    .collect::<Option<Vec<_>>>();
                let t1 = Instant::now();
                let Some(expanded) = expanded else { continue };
                let Some(u) = jts::union(&expanded) else {
                    continue;
                };
                let t2 = Instant::now();
                let Some(merged) = jts::buffer(&u, -*distance) else {
                    continue;
                };
                let t3 = Instant::now();
                let envelope = merged
                    .bounding_rect()
                    .map_or(0., |r| r.width() * r.height());
                let rings = if envelope < min_area {
                    vec![]
                } else {
                    jts::snap_and_fix(&merged)
                        .map(from_grid_polygons)
                        .unwrap_or_else(|| from_polygons(merged.clone()))
                };
                let t4 = Instant::now();
                // largest exterior area out
                let out_max = to_polygons(&rings)
                    .0
                    .iter()
                    .map(|p| Polygon::new(p.exterior().clone(), vec![]).unsigned_area())
                    .fold(0., f64::max);
                let dropped = filter_tile_polygon_areas(rings, min_area, min_area).is_empty();
                // bounds
                let t5 = Instant::now();
                let all = geo::MultiPoint::from(
                    cluster
                        .iter()
                        .flat_map(|g| {
                            g.0.iter()
                                .flat_map(|p| p.exterior().0.iter().map(|c| geo::Point::from(*c)))
                        })
                        .collect::<Vec<_>>(),
                );
                let hull = all.convex_hull();
                let hull_area = hull.unsigned_area();
                let t6 = Instant::now();
                let perimeter: f64 = hull
                    .exterior()
                    .0
                    .windows(2)
                    .map(|w| (w[1].x - w[0].x).hypot(w[1].y - w[0].y))
                    .sum();
                let hull_bound = hull_area + perimeter * 1. + 4.;
                let env = all.bounding_rect().map_or(0., |r| r.width() * r.height());
                let sum_buffered: f64 = expanded.iter().map(|g| g.unsigned_area()).sum();
                // Sound bounds from the buffered members (see findings): the
                // unbuffered union lies in the erosion of any convex superset.
                let t7 = Instant::now();
                let dd = *distance - 1.;
                let bpts = geo::MultiPoint::from(
                    expanded
                        .iter()
                        .flat_map(|g| {
                            g.0.iter()
                                .flat_map(|p| p.exterior().0.iter().map(|c| geo::Point::from(*c)))
                        })
                        .collect::<Vec<_>>(),
                );
                let bhull = bpts.convex_hull();
                let bhull_area = bhull.unsigned_area();
                let iso_bound = (bhull_area.sqrt() - dd * std::f64::consts::PI.sqrt())
                    .max(0.)
                    .powi(2);
                // The hull eroded by the distance less 0.8 (rounding and input
                // simplification slack): its edges' half-planes moved inwards.
                // Andrew's monotone chain, strictly convex, counter-clockwise.
                let chain = {
                    let mut pts: Vec<Coord<f64>> = bpts.0.iter().map(|p| p.0).collect();
                    pts.sort_unstable_by(|a, b| a.x.total_cmp(&b.x).then(a.y.total_cmp(&b.y)));
                    pts.dedup();
                    let cross = |o: Coord<f64>, a: Coord<f64>, b: Coord<f64>| {
                        (a.x - o.x) * (b.y - o.y) - (a.y - o.y) * (b.x - o.x)
                    };
                    let mut h: Vec<Coord<f64>> = Vec::with_capacity(pts.len() + 1);
                    for pass in 0..2 {
                        let start = h.len();
                        let it: Box<dyn Iterator<Item = &Coord<f64>>> = if pass == 0 {
                            Box::new(pts.iter())
                        } else {
                            Box::new(pts.iter().rev())
                        };
                        for &p in it {
                            while h.len() >= start + 2
                                && cross(h[h.len() - 2], h[h.len() - 1], p) <= 0.
                            {
                                h.pop();
                            }
                            h.push(p);
                        }
                        h.pop();
                    }
                    let first = h[0];
                    h.push(first);
                    h
                };
                let erode = |amount: f64| {
                    let ring = &chain;
                    let sign = 1.;
                    let mut poly: Vec<Coord<f64>> = ring[..ring.len() - 1].to_vec();
                    for e in ring.windows(2) {
                        let (dx, dy) = (e[1].x - e[0].x, e[1].y - e[0].y);
                        let len = dx.hypot(dy);
                        if len == 0. {
                            continue;
                        }
                        let (nx, ny) = (-dy / len * sign, dx / len * sign);
                        let side =
                            |c: &Coord<f64>| (c.x - e[0].x) * nx + (c.y - e[0].y) * ny - amount;
                        let mut next = Vec::with_capacity(poly.len() + 1);
                        for i in 0..poly.len() {
                            let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
                            let (sp, sq) = (side(&p), side(&q));
                            if sp >= 0. {
                                next.push(p);
                            }
                            if (sp >= 0.) != (sq >= 0.) {
                                let t = sp / (sp - sq);
                                next.push(Coord {
                                    x: p.x + t * (q.x - p.x),
                                    y: p.y + t * (q.y - p.y),
                                });
                            }
                        }
                        poly = next;
                        if poly.len() < 3 {
                            return 0.;
                        }
                    }
                    let mut twice = 0.;
                    for i in 0..poly.len() {
                        let (p, q) = (poly[i], poly[(i + 1) % poly.len()]);
                        twice += p.x * q.y - q.x * p.y;
                    }
                    twice.abs() / 2.
                };
                let env_bound = erode(*distance - 0.8);
                let iso_bound = iso_bound.min(erode(*distance - 2.));
                let t8 = Instant::now();
                let size = usize::from(members.len() > 2) + usize::from(members.len() > 5);
                for (k, bound) in [env_bound, iso_bound, env_bound.min(iso_bound)]
                    .into_iter()
                    .enumerate()
                {
                    if bound <= min_area {
                        sound[size][k].0 += 1;
                        sound[size][k].1 += (t4 - t1).as_secs_f64();
                        if !dropped {
                            sound[size][k].2 += 1;
                        }
                    }
                    if out_max > bound {
                        sound[size][k].3 += 1;
                    }
                }
                sound_cost += (t8 - t7).as_secs_f64();
                if out_max > env_bound.min(iso_bound) + 1e-9 {
                    println!(
                        "VIOLATION members {} out_max {out_max} hull-erode(d-0.8) {env_bound} other {iso_bound} hull {bhull_area}",
                        members.len()
                    );
                    println!(
                        "INPUT {}",
                        serde_json::to_string(
                            &cluster
                                .iter()
                                .map(|g| g
                                    .0
                                    .iter()
                                    .map(|p| std::iter::once(p.exterior())
                                        .chain(p.interiors())
                                        .map(|r| r.0.iter().map(|c| [c.x, c.y]).collect::<Vec<_>>())
                                        .collect::<Vec<_>>())
                                    .collect::<Vec<_>>())
                                .collect::<Vec<_>>()
                        )
                        .unwrap()
                    );
                    for (name, g) in [("UNION", &u), ("MERGED", &merged)] {
                        println!(
                            "{name} {}",
                            serde_json::to_string(
                                &g.0.iter()
                                    .map(|p| std::iter::once(p.exterior())
                                        .chain(p.interiors())
                                        .map(|r| r.0.iter().map(|c| [c.x, c.y]).collect::<Vec<_>>())
                                        .collect::<Vec<_>>())
                                    .collect::<Vec<_>>()
                            )
                            .unwrap()
                        );
                    }
                }
                let bucket = match members.len() {
                    2 => 0,
                    3..=5 => 1,
                    6..=20 => 2,
                    21..=100 => 3,
                    _ => 4,
                };
                let row = &mut rows[bucket][usize::from(dropped)];
                row.clusters += 1;
                row.buffer += (t1 - t0).as_secs_f64();
                row.union += (t2 - t1).as_secs_f64();
                row.unbuffer += (t3 - t2).as_secs_f64();
                row.snap += (t4 - t3).as_secs_f64();
                row.hull += (t6 - t5).as_secs_f64();
                if hull_bound <= min_area {
                    row.hull_pred += 1;
                    row.hull_pred_secs += (t4 - t0).as_secs_f64();
                    if !dropped {
                        violations += 1;
                    }
                }
                if env <= min_area {
                    row.env_pred += 1;
                }
                if sum_buffered <= min_area {
                    row.sum_pred += 1;
                }
                {
                    let secs = (t4 - t0).as_secs_f64();
                    let size = usize::from(members.len() > 2) + usize::from(members.len() > 5);
                    for (k, t) in THRESHOLDS.iter().enumerate() {
                        if hull_bound <= *t * min_area {
                            sweep[size][k].0 += 1;
                            sweep[size][k].1 += secs;
                            sweep[size][k].2 = sweep[size][k].2.max(out_max);
                        }
                    }
                }
                if hull_area > 0. {
                    let ratio = out_max / hull_area;
                    worst_ratio = worst_ratio.max(ratio);
                    worst_excess = worst_excess.max(out_max - hull_bound);
                    let slot = ((ratio * 10.) as usize).min(11);
                    ratio_hist[slot] += 1;
                }
            }
        }
        println!(
            "proximity {proximity:.2}s; singletons {singles}, of which dropped by area {singles_dropped}"
        );
        println!(
            "bucket dropped clusters buffer union unbuffer snap | hull_time hull_pred hull_pred_secs env_pred sumbuf_pred"
        );
        for (b, name) in ["2", "3-5", "6-20", "21-100", ">100"].iter().enumerate() {
            for (d, r) in rows[b].iter().enumerate() {
                println!(
                    "{name:>7} {d} {:>7} {:6.2} {:6.2} {:6.2} {:6.2} | {:5.2} {:>7} {:6.2} {:>7} {:>7}",
                    r.clusters,
                    r.buffer,
                    r.union,
                    r.unbuffer,
                    r.snap,
                    r.hull,
                    r.hull_pred,
                    r.hull_pred_secs,
                    r.env_pred,
                    r.sum_pred
                );
            }
        }
        for (size, name) in ["2", "3-5", ">5"].iter().enumerate() {
            for (k, t) in THRESHOLDS.iter().enumerate() {
                let (n, secs, max_out) = sweep[size][k];
                println!(
                    "members {name}: hull_bound <= {t} x min_area: {n} clusters, {secs:.2}s, largest output exterior {:.3} x min_area",
                    max_out / min_area
                );
            }
        }
        for (size, name) in ["2", "3-5", ">5"].iter().enumerate() {
            for (k, bound) in ["hull eroded d-0.8", "iso or hull eroded d-2", "either"]
                .iter()
                .enumerate()
            {
                let (n, secs, wrong, exceeded) = sound[size][k];
                println!(
                    "sound {bound}, members {name}: {n} clusters skip union+unbuffer+snap, {secs:.2}s saved, {wrong} wrongly skipped, {exceeded} outputs above the bound"
                );
            }
        }
        println!("sound bounds cost {sound_cost:.2}s");
        println!(
            "hull bound violations {violations}; worst out/hull area ratio {worst_ratio:.4}; worst out - hull_bound {worst_excess:.2}; ratio histogram {ratio_hist:?}"
        );
    }

    /// Checks the early-out bounds on every merged cluster of the dumped
    /// groups: no polygon the full dissolve keeps may have an exterior above
    /// either bound, and reports how many clusters each bound skips and the
    /// single-thread time the dissolve spends on them.
    #[test]
    #[ignore]
    fn merge_replay_drop_bounds() {
        use super::super::jts_buffer as jts;
        let Some(groups) = groups() else { return };
        let min_area: f64 = std::env::var("OTC_BENCH_MIN_AREA")
            .ok()
            .map_or(4., |v| v.parse().unwrap())
            * 256.;
        let overlay_buffer = |g: &MultiPolygon<f64>, amount: f64| {
            g.buffer_with_style(
                BufferStyle::new(amount).line_join(LineJoin::Miter(2. * (1_f64 / 5.).asin())),
            )
        };
        let (mut merged_clusters, mut fallbacks) = (0usize, 0usize);
        let (mut before_skips, mut after_skips) = (0usize, 0usize);
        let (mut before_secs, mut after_secs, mut total_secs) = (0., 0., 0.);
        let (mut bound_secs, mut buffer_secs_skipped) = (0., 0.);
        let (mut above_before, mut above_after, mut wrong) = (0usize, 0usize, 0usize);
        let (mut worst_before, mut worst_after) = (0_f64, 0_f64);
        for (distance, geoms) in &groups {
            if *distance <= 1. {
                continue;
            }
            for (members, ..) in merge_nearby_collect(geoms, *distance) {
                if members.len() == 1 {
                    continue;
                }
                merged_clusters += 1;
                let cluster: Vec<MultiPolygon<f64>> =
                    members.iter().map(|&i| geoms[i].clone()).collect();
                let t0 = Instant::now();
                let before = jts::pre_buffer_bound(&cluster, *distance);
                let t1 = Instant::now();
                let after = jts::post_buffer_bound(&cluster, *distance);
                for g in &cluster {
                    let _ = jts::buffer(g, *distance);
                }
                let t2 = Instant::now();
                let merged = match jts::buffer_union_unbuffer(&cluster, *distance, 0.) {
                    Some(m) => m,
                    None => {
                        fallbacks += 1;
                        overlay_buffer_union_unbuffer(&cluster, *distance, overlay_buffer)
                    }
                };
                let envelope = merged
                    .bounding_rect()
                    .map_or(0., |r| r.width() * r.height());
                let rings = if envelope < min_area {
                    vec![]
                } else {
                    jts::snap_and_fix(&merged)
                        .map(from_grid_polygons)
                        .unwrap_or_else(|| from_polygons(merged.clone()))
                };
                let t3 = Instant::now();
                let kept = filter_tile_polygon_areas(rings, min_area, min_area);
                let out_max = to_polygons(&kept)
                    .0
                    .iter()
                    .map(|p| Polygon::new(p.exterior().clone(), vec![]).unsigned_area())
                    .fold(0., f64::max);
                let secs = (t3 - t2).as_secs_f64() + (t2 - t1).as_secs_f64();
                total_secs += secs;
                bound_secs += (t1 - t0).as_secs_f64();
                let after = after.unwrap_or(f64::INFINITY);
                if out_max > before + 1e-9 {
                    above_before += 1;
                }
                if out_max > after + 1e-9 {
                    above_after += 1;
                }
                if out_max > 0. {
                    worst_before = worst_before.max(out_max / before);
                    worst_after = worst_after.max(out_max / after);
                }
                if before <= min_area {
                    before_skips += 1;
                    before_secs += secs;
                    buffer_secs_skipped += (t2 - t1).as_secs_f64();
                    if !kept.is_empty() {
                        wrong += 1;
                    }
                } else if after <= min_area {
                    after_skips += 1;
                    after_secs += secs - (t2 - t1).as_secs_f64();
                    if !kept.is_empty() {
                        wrong += 1;
                    }
                }
            }
        }
        println!(
            "{merged_clusters} merged clusters, {fallbacks} through the overlay fallback, {total_secs:.2}s buffer to snap"
        );
        println!(
            "before buffering: {before_skips} skipped, {before_secs:.2}s saved, of which buffer {buffer_secs_skipped:.2}s"
        );
        println!("after buffering only: {after_skips} more skipped, {after_secs:.2}s saved");
        println!("bound before buffering costs {bound_secs:.2}s");
        println!(
            "outputs above the bound before buffering {above_before}, after {above_after}; wrongly skipped {wrong}; worst output/bound {worst_before:.4} and {worst_after:.4}"
        );
        assert_eq!((above_before, above_after, wrong), (0, 0, 0));
    }

    /// The proximity clusters' members, without merging them.
    fn merge_nearby_collect(
        geoms: &[MultiPolygon<f64>],
        distance: f64,
    ) -> Vec<(Vec<usize>, MultiPolygon<f64>, bool)> {
        COLLECT_ONLY.with(|c| c.set(true));
        let result = merge_nearby(geoms.to_vec(), distance);
        COLLECT_ONLY.with(|c| c.set(false));
        result
    }

    thread_local! {
        /// Makes `merge_nearby` return each cluster's members unmerged.
        pub(super) static COLLECT_ONLY: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
    }
}
