//! Reads a search index and answers queries.
use super::fold::{fold, words};
use super::{
    ATTR_BYTES, ATTRS, DICT, DICT_INDEX, HAS_ELE, HAS_ID, HAS_REF, HEADER_BYTES, KINDS, Kind,
    MAGIC, META, NO_CONTEXT, POINT_BYTES, POINTS, POSTINGS, RECORD_BLOCKS, RECORDS,
    RECORDS_PER_BLOCK, SEARCH_INDEX_VERSION, SECTIONS, distance_m, unzigzag, varint,
};
use crate::{CompileBounds, CompileError};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

/// Words a query may hold that names in Poland mostly leave out: they are
/// dropped from a query that has other words.
const OPTIONAL_WORDS: [&str; 9] = [
    "ul", "ulica", "al", "aleja", "aleje", "pl", "plac", "os", "osiedle",
];
/// A prefix matching more words than this uses the ones whose best record
/// is most prominent.
const MAX_EXPANSION: usize = 512;
/// Records one query word may match before the rest are left out, the less
/// prominent first.
const MAX_WORD_RECORDS: usize = 500_000;
/// Records a query weighs without a place to be near, most prominent first.
const MAX_RANKED: usize = 20_000;
/// The same, with `near` or `within`, which need every record's place.
const MAX_RANKED_SPATIAL: usize = 300_000;
/// Reads of fixed rows closer than this are made as one.
const READ_GAP: u64 = 4096;
/// Equal names of one kind within this distance are one hit.
const SAME_PLACE_M: f64 = 300.0;

/// A query of a [`SearchIndex`].
#[derive(Debug, Clone, PartialEq)]
#[non_exhaustive]
pub struct SearchQuery {
    /// What to look for: words of a name, in any case and with or without
    /// diacritics. The last word matches as a prefix unless the text ends in
    /// a space. A word may also match the nearest town or village, so
    /// `dluga krakow` finds the streets called Długa in Kraków.
    pub text: String,
    /// Longitude and latitude to rank by: nearer hits come first among
    /// equally good ones [default: none].
    pub near: Option<[f64; 2]>,
    /// Only hits inside this box [default: anywhere].
    pub within: Option<CompileBounds>,
    /// Only hits from these archive layers (`place`, `poi`,
    /// `transportation_name`, `sar`...) [default: all].
    pub layers: Vec<String>,
    /// The most hits to return [default: 10].
    pub limit: usize,
}

impl SearchQuery {
    pub fn new(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            near: None,
            within: None,
            layers: Vec::new(),
            limit: 10,
        }
    }
}

/// One record a query found.
#[derive(Debug, Clone, PartialEq, serde::Serialize)]
#[non_exhaustive]
pub struct SearchHit {
    /// The `name` the map shows.
    pub name: String,
    /// Its other names that fold differently: `name_en`, `name_de`,
    /// `name_int`, `name:latin`.
    pub other_names: Vec<String>,
    /// The archive layer, class and subclass the record comes from, such as
    /// `poi`, `shop`, `bakery`. Joined streets have the class of their most
    /// important piece.
    pub layer: String,
    pub class: String,
    pub subclass: String,
    pub longitude: f64,
    pub latitude: f64,
    /// The name of the nearest city, town or village, for anything else.
    pub context: Option<String>,
    /// Metres from the query's `near` point.
    pub distance_m: Option<f64>,
    /// Elevation in metres, for peaks and SAR points that have one.
    pub ele: Option<i64>,
    /// A reference: an emergency access point's or helipad's `ref`, an
    /// airport's IATA or ICAO code.
    pub reference: Option<String>,
    /// The feature id in the archive's tiles (the OSM id × 10, plus 1 for
    /// a node, 2 for a way, 3 for a relation), for points that have one.
    pub feature_id: Option<u64>,
    /// Higher is better; only the order within one query means anything.
    pub score: f64,
}

/// A search index opened for queries. It holds the file open and keeps only
/// its header, kinds and dictionary index in memory; queries read the rest
/// as they need it, and can run from several threads at once.
pub struct SearchIndex {
    source: Source,
    sections: [(u64, u64); SECTIONS],
    records: u32,
    kinds: Vec<Kind>,
    metadata: serde_json::Value,
    blocks: Vec<DictBlock>,
}

impl std::fmt::Debug for SearchIndex {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SearchIndex")
            .field("records", &self.records)
            .field("metadata", &self.metadata)
            .finish_non_exhaustive()
    }
}

enum Source {
    File { file: Mutex<File>, length: u64 },
    Bytes(Vec<u8>),
}

impl Source {
    fn length(&self) -> u64 {
        match self {
            Self::File { length, .. } => *length,
            Self::Bytes(bytes) => bytes.len() as u64,
        }
    }

    fn read(&self, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        if offset
            .checked_add(length)
            .is_none_or(|end| end > self.length())
        {
            return Err(format!(
                "the search index is cut short: bytes {offset}..+{length} are past its end"
            ));
        }
        match self {
            Self::Bytes(bytes) => Ok(bytes[offset as usize..(offset + length) as usize].to_vec()),
            Self::File { file, .. } => {
                let mut bytes = vec![0; length as usize];
                let mut file = file.lock().unwrap_or_else(|e| e.into_inner());
                file.seek(SeekFrom::Start(offset))
                    .and_then(|_| file.read_exact(&mut bytes))
                    .map_err(|e| format!("Failed to read the search index: {e}"))?;
                Ok(bytes)
            }
        }
    }
}

struct DictBlock {
    dict: u64,
    postings: u64,
    first: Box<[u8]>,
}

#[derive(Debug, Clone)]
struct DictEntry {
    key: Vec<u8>,
    count: u64,
    first: u32,
    /// Where the rest of the record ids start in the postings section.
    postings: u64,
    rest: u64,
}

/// A record's variable part.
struct RecordData {
    names: Vec<String>,
    ele: Option<i64>,
    reference: Option<String>,
    id: Option<u64>,
}

struct Candidate {
    id: u32,
    kind: u16,
    context: u32,
    score: f64,
    point: Option<[f64; 2]>,
    distance_m: Option<f64>,
}

impl SearchIndex {
    /// Opens the index at `path`.
    pub fn open(path: &Path) -> Result<Self, CompileError> {
        let failed = |e: std::io::Error| {
            CompileError::other(format!("Failed to open {}: {e}", path.display()))
        };
        let file = File::open(path).map_err(failed)?;
        let length = file.metadata().map_err(failed)?.len();
        Self::from_source(Source::File {
            file: Mutex::new(file),
            length,
        })
        .map_err(|e| CompileError::other(format!("{}: {e}", path.display())))
    }

    /// An index held in memory, such as one bundled with an app.
    pub fn from_bytes(bytes: Vec<u8>) -> Result<Self, CompileError> {
        Self::from_source(Source::Bytes(bytes)).map_err(CompileError::other)
    }

    fn from_source(source: Source) -> Result<Self, String> {
        if source.length() < HEADER_BYTES as u64 {
            return Err("not a search index: too short".into());
        }
        let head = source.read(0, HEADER_BYTES as u64)?;
        if &head[..8] != MAGIC {
            return Err("not a search index".into());
        }
        let version = u32::from_le_bytes(head[8..12].try_into().unwrap());
        if version != SEARCH_INDEX_VERSION {
            return Err(format!(
                "search index version {version}; this build reads version {SEARCH_INDEX_VERSION}"
            ));
        }
        let count = u32::from_le_bytes(head[12..16].try_into().unwrap()) as usize;
        if count != SECTIONS {
            return Err(format!("search index has {count} sections, not {SECTIONS}"));
        }
        let mut sections = [(0, 0); SECTIONS];
        for (i, section) in sections.iter_mut().enumerate() {
            let at = 16 + i * 16;
            let offset = u64::from_le_bytes(head[at..at + 8].try_into().unwrap());
            let length = u64::from_le_bytes(head[at + 8..at + 16].try_into().unwrap());
            if offset
                .checked_add(length)
                .is_none_or(|end| end > source.length())
            {
                return Err("the search index is cut short".into());
            }
            *section = (offset, length);
        }
        let read = |i: usize| source.read(sections[i].0, sections[i].1);
        let metadata: serde_json::Value = serde_json::from_slice(&read(META)?)
            .map_err(|e| format!("search index metadata: {e}"))?;
        let kinds: Vec<(String, String, String, u8)> = serde_json::from_slice(&read(KINDS)?)
            .map_err(|e| format!("search index kinds: {e}"))?;
        let kinds = kinds
            .into_iter()
            .map(|(layer, class, subclass, tier)| Kind {
                tier,
                layer,
                class,
                subclass,
            })
            .collect();
        let records = sections[POINTS].1 / POINT_BYTES as u64;
        let blocks_expected = records.div_ceil(RECORDS_PER_BLOCK as u64) + 1;
        if sections[POINTS].1 % POINT_BYTES as u64 != 0
            || sections[ATTRS].1 != records * ATTR_BYTES as u64
            || sections[RECORD_BLOCKS].1 != blocks_expected * 4
            || records >= u64::from(NO_CONTEXT)
        {
            return Err("the search index's record tables disagree".into());
        }
        let index = read(DICT_INDEX)?;
        let mut blocks = Vec::new();
        let mut at = 0;
        while at < index.len() {
            let fixed = index
                .get(at..at + 8)
                .ok_or("the search index's dictionary index is cut short")?;
            let dict = u64::from(u32::from_le_bytes(fixed[..4].try_into().unwrap()));
            let postings = u64::from(u32::from_le_bytes(fixed[4..].try_into().unwrap()));
            at += 8;
            let length = varint(&index, &mut at)?;
            let first = take(&index, &mut at, length)
                .ok_or("the search index's dictionary index is cut short")?;
            if dict > sections[DICT].1 || postings > sections[POSTINGS].1 {
                return Err("the search index's dictionary index points past its end".into());
            }
            blocks.push(DictBlock {
                dict,
                postings,
                first: first.into(),
            });
        }
        Ok(Self {
            source,
            sections,
            records: records as u32,
            kinds,
            metadata,
            blocks,
        })
    }

    /// Records in the index.
    pub fn len(&self) -> u64 {
        u64::from(self.records)
    }

    pub fn is_empty(&self) -> bool {
        self.records == 0
    }

    /// The index's metadata: `format`, `version`, `records`, the `source`
    /// archive's file name, the `zoom` read, the archive's `bounds` and
    /// `osm_data_timestamp`.
    pub fn metadata(&self) -> &serde_json::Value {
        &self.metadata
    }

    /// The hits for `query`, best first.
    pub fn search(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, CompileError> {
        self.run(query).map_err(CompileError::other)
    }

    fn section(&self, section: usize, offset: u64, length: u64) -> Result<Vec<u8>, String> {
        let (start, size) = self.sections[section];
        if offset.checked_add(length).is_none_or(|end| end > size) {
            return Err("the search index points past the end of a section".into());
        }
        self.source.read(start + offset, length)
    }

    fn block_entries(&self, block: usize) -> Result<Vec<DictEntry>, String> {
        let start = self.blocks[block].dict;
        let end = self
            .blocks
            .get(block + 1)
            .map_or(self.sections[DICT].1, |next| next.dict);
        let bytes = self.section(DICT, start, end.saturating_sub(start))?;
        let mut entries: Vec<DictEntry> = Vec::new();
        let mut postings = self.blocks[block].postings;
        let mut at = 0;
        while at < bytes.len() {
            let shared = varint(&bytes, &mut at)? as usize;
            let suffix = varint(&bytes, &mut at)?;
            let mut key = match entries.last() {
                Some(previous) if shared <= previous.key.len() => previous.key[..shared].to_vec(),
                None if shared == 0 => Vec::new(),
                _ => return Err("the search index's dictionary is malformed".into()),
            };
            key.extend(
                take(&bytes, &mut at, suffix)
                    .ok_or("the search index's dictionary is cut short")?,
            );
            let count = varint(&bytes, &mut at)?;
            let first = u32::try_from(varint(&bytes, &mut at)?).map_err(|e| e.to_string())?;
            let rest = varint(&bytes, &mut at)?;
            entries.push(DictEntry {
                key,
                count,
                first,
                postings,
                rest,
            });
            postings = postings
                .checked_add(rest)
                .ok_or("the search index's dictionary is malformed")?;
        }
        Ok(entries)
    }

    /// The block that would hold `key`.
    fn block_for(&self, key: &[u8]) -> usize {
        self.blocks
            .partition_point(|block| &*block.first <= key)
            .saturating_sub(1)
    }

    fn exact(&self, word: &str) -> Result<Option<DictEntry>, String> {
        if self.blocks.is_empty() {
            return Ok(None);
        }
        let key = word.as_bytes();
        Ok(self
            .block_entries(self.block_for(key))?
            .into_iter()
            .find(|entry| entry.key == key))
    }

    /// The words that start with `prefix`, at most [`MAX_EXPANSION`].
    fn prefixed(&self, prefix: &str) -> Result<Vec<DictEntry>, String> {
        let key = prefix.as_bytes();
        let mut found = Vec::new();
        if self.blocks.is_empty() {
            return Ok(found);
        }
        'blocks: for block in self.block_for(key)..self.blocks.len() {
            for entry in self.block_entries(block)? {
                if entry.key.starts_with(key) {
                    found.push(entry);
                } else if entry.key.as_slice() > key {
                    break 'blocks;
                }
            }
        }
        if found.len() > MAX_EXPANSION {
            found.sort_by_key(|entry| entry.first);
            found.truncate(MAX_EXPANSION);
        }
        Ok(found)
    }

    fn ids(&self, entry: &DictEntry) -> Result<Vec<u32>, String> {
        let bytes = self.section(POSTINGS, entry.postings, entry.rest)?;
        let mut ids = Vec::with_capacity(entry.count.min(entry.rest + 1) as usize);
        ids.push(entry.first);
        let mut at = 0;
        let mut id = entry.first;
        while at < bytes.len() {
            let delta = u32::try_from(varint(&bytes, &mut at)?).map_err(|e| e.to_string())?;
            id = id
                .checked_add(delta)
                .ok_or("the search index's postings overflow")?;
            ids.push(id);
        }
        if ids.len() as u64 != entry.count || ids.last().is_some_and(|&id| id >= self.records) {
            return Err("the search index's postings are malformed".into());
        }
        Ok(ids)
    }

    /// The records of `entries`, sorted, at most [`MAX_WORD_RECORDS`].
    fn union(&self, mut entries: Vec<DictEntry>) -> Result<Vec<u32>, String> {
        entries.sort_by_key(|entry| entry.first);
        let mut ids = Vec::new();
        for entry in &entries {
            if ids.len() >= MAX_WORD_RECORDS {
                break;
            }
            ids.extend(self.ids(entry)?);
        }
        ids.sort_unstable();
        ids.dedup();
        ids.truncate(MAX_WORD_RECORDS);
        Ok(ids)
    }

    /// The fixed rows of `ids`, which are sorted, in their order: reads
    /// close together are made as one.
    fn rows(&self, section: usize, width: usize, ids: &[u32]) -> Result<Vec<u8>, String> {
        let width64 = width as u64;
        let mut out = Vec::with_capacity(ids.len() * width);
        let mut i = 0;
        while i < ids.len() {
            let start = u64::from(ids[i]) * width64;
            let mut j = i + 1;
            while j < ids.len()
                && u64::from(ids[j]) * width64 <= u64::from(ids[j - 1]) * width64 + READ_GAP
            {
                j += 1;
            }
            let end = u64::from(ids[j - 1]) * width64 + width64;
            let bytes = self.section(section, start, end - start)?;
            for &id in &ids[i..j] {
                let at = (u64::from(id) * width64 - start) as usize;
                out.extend(&bytes[at..at + width]);
            }
            i = j;
        }
        Ok(out)
    }

    fn point(&self, id: u32) -> Result<[f64; 2], String> {
        let row = self.rows(POINTS, POINT_BYTES, &[id])?;
        Ok(decode_point(&row))
    }

    fn record(&self, id: u32) -> Result<RecordData, String> {
        let block = id as usize / RECORDS_PER_BLOCK;
        let bounds = self.section(RECORD_BLOCKS, block as u64 * 4, 8)?;
        let start = u64::from(u32::from_le_bytes(bounds[..4].try_into().unwrap()));
        let end = u64::from(u32::from_le_bytes(bounds[4..].try_into().unwrap()));
        let bytes = self.section(RECORDS, start, end.saturating_sub(start))?;
        let mut at = 0;
        for _ in 0..id as usize % RECORDS_PER_BLOCK {
            parse_record(&bytes, &mut at)?;
        }
        parse_record(&bytes, &mut at)
    }

    fn run(&self, query: &SearchQuery) -> Result<Vec<SearchHit>, String> {
        let mut words = words(&query.text);
        if words.is_empty() || query.limit == 0 {
            return Ok(Vec::new());
        }
        let last_is_prefix = !query.text.ends_with(char::is_whitespace);
        if words.iter().any(|w| !OPTIONAL_WORDS.contains(&w.as_str())) {
            let count = words.len();
            let mut index = 0;
            words.retain(|word| {
                index += 1;
                // A word being typed stays: it may be the start of another.
                !OPTIONAL_WORDS.contains(&word.as_str()) || (index == count && last_is_prefix)
            });
        }

        // The records each word matches by their own names.
        let mut terms: Vec<Vec<u32>> = Vec::new();
        let mut exact_last: Vec<u32> = Vec::new();
        for (index, word) in words.iter().enumerate() {
            let last = index + 1 == words.len();
            let exact = self.exact(word)?;
            let entries = if last && last_is_prefix {
                self.prefixed(word)?
            } else {
                match &exact {
                    Some(entry) => vec![entry.clone()],
                    // A word typed short: match it as a prefix.
                    None => self.prefixed(word)?,
                }
            };
            if last && let Some(entry) = &exact {
                exact_last = self.ids(entry)?;
            }
            let ids = self.union(entries)?;
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            terms.push(ids);
        }

        let spatial = query.near.is_some() || query.within.is_some();
        let cap = if spatial {
            MAX_RANKED_SPATIAL
        } else {
            MAX_RANKED
        };
        let mut candidates: Vec<u32> = terms.iter().flatten().copied().collect();
        candidates.sort_unstable();
        candidates.dedup();
        candidates.truncate(cap);
        candidates.extend(exact_last.iter().take(cap));
        candidates.sort_unstable();
        candidates.dedup();

        let attrs = self.rows(ATTRS, ATTR_BYTES, &candidates)?;
        let mut kept = Vec::new();
        for (&id, row) in candidates.iter().zip(attrs.as_chunks::<ATTR_BYTES>().0) {
            let kind = u16::from_le_bytes([row[0], row[1]]);
            let context = u32::from_le_bytes(row[2..6].try_into().unwrap());
            let rank = row[6];
            let info = self
                .kinds
                .get(usize::from(kind))
                .ok_or("the search index names a kind it does not list")?;
            if !query.layers.is_empty() && !query.layers.contains(&info.layer) {
                continue;
            }
            let mut own = 0;
            let all = terms.iter().all(|ids| {
                if ids.binary_search(&id).is_ok() {
                    own += 1;
                    true
                } else {
                    context != NO_CONTEXT && ids.binary_search(&context).is_ok()
                }
            });
            if !all || own == 0 {
                continue;
            }
            let mut score = -f64::from(rank) + 3.0 * f64::from(own);
            if exact_last.binary_search(&id).is_ok() {
                score += 4.0;
            }
            kept.push(Candidate {
                id,
                kind,
                context,
                score,
                point: None,
                distance_m: None,
            });
        }

        if spatial {
            let ids: Vec<u32> = kept.iter().map(|c| c.id).collect();
            let points = self.rows(POINTS, POINT_BYTES, &ids)?;
            for (candidate, row) in kept.iter_mut().zip(points.as_chunks::<POINT_BYTES>().0) {
                candidate.point = Some(decode_point(row));
            }
            if let Some(bounds) = query.within {
                kept.retain(|c| c.point.is_some_and(|[lon, lat]| bounds.contains(lon, lat)));
            }
            if let Some(near) = query.near {
                for candidate in &mut kept {
                    let d = distance_m(near, candidate.point.expect("read above"));
                    candidate.distance_m = Some(d);
                    candidate.score -= 6.0 * (1.0 + d / 1000.0).log2();
                }
            }
        }
        let by_score =
            |a: &Candidate, b: &Candidate| b.score.total_cmp(&a.score).then(a.id.cmp(&b.id));
        kept.sort_by(by_score);
        kept.truncate(query.limit * 5 + 20);

        // Whole-name matches, which need the names.
        let folded_query = fold(&query.text);
        let mut data = Vec::with_capacity(kept.len());
        for candidate in &mut kept {
            let record = self.record(candidate.id)?;
            let best = record
                .names
                .iter()
                .map(|name| {
                    let name = fold(name);
                    if name == folded_query {
                        15.0
                    } else if name.starts_with(&folded_query) {
                        6.0
                    } else {
                        0.0
                    }
                })
                .fold(0.0, f64::max);
            candidate.score += best;
            data.push((candidate.id, record));
        }
        kept.sort_by(by_score);

        let mut hits: Vec<SearchHit> = Vec::new();
        for candidate in kept {
            if hits.len() == query.limit {
                break;
            }
            let index = data
                .iter()
                .position(|(id, _)| *id == candidate.id)
                .expect("every candidate was read");
            let record = data.swap_remove(index).1;
            let kind = &self.kinds[usize::from(candidate.kind)];
            let [longitude, latitude] = match candidate.point {
                Some(point) => point,
                None => self.point(candidate.id)?,
            };
            let mut names = record.names.into_iter();
            let name = names.next().unwrap_or_default();
            let same = hits.iter().any(|hit| {
                hit.layer == kind.layer
                    && hit.class == kind.class
                    && hit.name == name
                    && distance_m([hit.longitude, hit.latitude], [longitude, latitude])
                        < SAME_PLACE_M
            });
            if same {
                continue;
            }
            let context = if candidate.context == NO_CONTEXT {
                None
            } else {
                self.record(candidate.context)?.names.into_iter().next()
            };
            hits.push(SearchHit {
                name,
                other_names: names.collect(),
                layer: kind.layer.clone(),
                class: kind.class.clone(),
                subclass: kind.subclass.clone(),
                longitude,
                latitude,
                context,
                distance_m: candidate.distance_m.map(|d| d.round()),
                ele: record.ele,
                reference: record.reference,
                feature_id: record.id,
                score: candidate.score,
            });
        }
        Ok(hits)
    }
}

/// The `length` bytes at `at`, moving `at` past them.
fn take<'a>(bytes: &'a [u8], at: &mut usize, length: u64) -> Option<&'a [u8]> {
    let end = at.checked_add(usize::try_from(length).ok()?)?;
    let taken = bytes.get(*at..end)?;
    *at = end;
    Some(taken)
}

fn decode_point(row: &[u8]) -> [f64; 2] {
    let lon = i32::from_le_bytes(row[..4].try_into().unwrap());
    let lat = i32::from_le_bytes(row[4..8].try_into().unwrap());
    [f64::from(lon) / 1e7, f64::from(lat) / 1e7]
}

fn parse_record(bytes: &[u8], at: &mut usize) -> Result<RecordData, String> {
    let text = |bytes: &[u8], at: &mut usize| -> Result<String, String> {
        let length = varint(bytes, at)?;
        let raw = take(bytes, at, length).ok_or("a search index record is cut short")?;
        String::from_utf8(raw.to_vec()).map_err(|_| "a search index name is not UTF-8".into())
    };
    let count = varint(bytes, at)? as usize;
    if count > bytes.len() {
        return Err("a search index record is malformed".into());
    }
    let mut names = Vec::with_capacity(count);
    for _ in 0..count {
        names.push(text(bytes, at)?);
    }
    let flags = *bytes.get(*at).ok_or("a search index record is cut short")?;
    *at += 1;
    let ele = if flags & HAS_ELE != 0 {
        Some(unzigzag(varint(bytes, at)?))
    } else {
        None
    };
    let reference = if flags & HAS_REF != 0 {
        Some(text(bytes, at)?)
    } else {
        None
    };
    let id = if flags & HAS_ID != 0 {
        Some(varint(bytes, at)?)
    } else {
        None
    };
    Ok(RecordData {
        names,
        ele,
        reference,
        id,
    })
}
