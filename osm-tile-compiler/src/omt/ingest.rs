//! Schema-independent OSM source reconstruction for the native OMT profile.
//! Incomplete ways are dropped rather than bridging missing nodes; relations
//! retain available closed rings, matching OsmMultipolygon's partial assembly.
//! Ring joins retain OSM node identity; nested relation members and the
//! reference's sub-metre damaged-ring repair remain explicit limitations.
use super::{
    Profile,
    input_area::{Extent, InputArea},
    pipeline::{Pipeline, lock},
    types::{Source, SourceKind, Tags},
};
use crate::control::{CompileStage, Control};
use geo::coordinate_position::CoordPos;
use geo::{Area, BoundingRect, Contains, CoordinatePosition};
use geo_types::{Coord, Geometry, LineString, MultiPolygon, Point, Polygon, Rect};
use osm_pbf_ingest::IngestStage;
use osm_pbf_ingest::blob_index::ObjectKind;
use osm_pbf_ingest::node_store::DecimicroBox;
use osm_pbf_ingest::{
    IngestBuildProfile, NodeBatch, NodeTags, NormalizedNode, NormalizedRelation, Pass2Options,
    Pass2Sink, PrepareOptions, PreparedIngestIndex, RelationBatch, RelationMemberType, WayBatch,
    WayRef, prepare_ingest_index_with_options, run_pass2_with_options,
};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicUsize, Ordering},
};

fn tags(input: &NodeTags) -> Tags {
    input
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect()
}
fn encoded_id(id: i64, kind: SourceKind) -> Result<u64, String> {
    let id = u64::try_from(id).map_err(|_| format!("Negative OSM ID {id} is unsupported"))?;
    id.checked_mul(10)
        .and_then(|v| {
            v.checked_add(match kind {
                SourceKind::Node => 1,
                SourceKind::Way => 2,
                SourceKind::Relation => 3,
                SourceKind::Supplement => 0,
            })
        })
        .ok_or_else(|| format!("OSM ID {id} overflows feature ID"))
}

fn skip_way_geometry(tags_empty: bool, parents_empty: bool, polygon_member: bool) -> bool {
    tags_empty && parents_empty && !polygon_member
}

// TransportationName.HighwayClass recognizes these six exact highway tags
// when assigning a class to a motorway junction. In particular, *_link ways
// do not participate even when their parent highway has a higher class.
fn junction_highway_rank(highway: &str) -> u8 {
    match highway {
        "motorway" => 6,
        "trunk" => 5,
        "primary" => 4,
        "secondary" => 3,
        "tertiary" => 2,
        "unclassified" => 1,
        _ => 0,
    }
}

// Match the four OMT preprocessOsmRelation handlers. OsmReader only adds way
// memberships to its multimap when at least one handler returns relation info.
fn profile_relation_eligible(relation: &NormalizedRelation) -> bool {
    let tag = |key| relation.tags.get(key);
    let building = tag("type") == Some("building");
    let waterway =
        tag("waterway") == Some("river") && tag("name").is_some_and(|name| !name.is_empty());
    let boundary = tag("type") == Some("boundary")
        && tag("boundary") == Some("administrative")
        && tag("admin_level")
            .and_then(java_parse_round_int)
            .is_some_and(|level| (2..=10).contains(&level));
    let route = matches!(tag("route"), Some("road" | "hiking"))
        && (relation.tags.contains_key("network")
            || relation.tags.contains_key("osmc:symbol")
            || relation.tags.contains_key("colour"));
    building || waterway || boundary || route
}

fn java_parse_round_int(value: &str) -> Option<i32> {
    // Parse.parseRoundInt uses ^(-?[\\d.]+)(\\D|$), Float.parseFloat,
    // and Math.round. The greedy capture is parsed once; malformed decimals
    // are not rescued by trying a shorter prefix.
    let bytes = value.as_bytes();
    let start = usize::from(bytes.first() == Some(&b'-'));
    let mut max = start;
    while bytes
        .get(max)
        .is_some_and(|byte| byte.is_ascii_digit() || *byte == b'.')
    {
        max += 1;
    }
    if max == start {
        return None;
    }
    let parsed = value[..max].parse::<f32>().ok()?;
    Some((parsed + 0.5).floor() as i32)
}

/// One way membership of an eligible relation, by index so the index can be
/// shared across threads together with the relations it describes.
#[derive(Clone, Copy)]
struct ProfileMembership {
    way_id: i64,
    relation: usize,
    member: usize,
}

struct ProfileMembershipIndex {
    entries: Vec<ProfileMembership>,
}

// Java Arrays.binarySearch returns the first midpoint it hits in the full
// sorted key array; it does not find the beginning of a run of equal keys.
fn java_duplicate_key_hit(total: usize, lower: usize, upper: usize) -> Option<usize> {
    if lower >= upper || upper > total {
        return None;
    }
    let mut low = 0usize;
    let mut high = total.checked_sub(1)?;
    while low <= high {
        let mid = low + (high - low) / 2;
        if mid < lower {
            low = mid + 1;
        } else if mid >= upper {
            high = mid.checked_sub(1)?;
        } else {
            return Some(mid);
        }
    }
    None
}

impl ProfileMembershipIndex {
    /// `relations_in_source_order` pairs each relation with its index.
    fn new<'a>(
        relations_in_source_order: impl IntoIterator<Item = (usize, &'a NormalizedRelation)>,
    ) -> Self {
        let mut entries = Vec::new();
        for (index, relation) in relations_in_source_order {
            if !profile_relation_eligible(relation) {
                continue;
            }
            for (member_index, member) in relation.members.iter().enumerate() {
                if member.member_type == RelationMemberType::Way {
                    entries.push(ProfileMembership {
                        way_id: member.member_id,
                        relation: index,
                        member: member_index,
                    });
                }
            }
        }
        // Java sorts only keys, filling corresponding values in original
        // append order for each key. Stable sort by way ID has that effect.
        entries.sort_by_key(|entry| entry.way_id);
        Self { entries }
    }

    fn get(&self, way_id: i64) -> Vec<&ProfileMembership> {
        let lower = self.entries.partition_point(|entry| entry.way_id < way_id);
        let upper = self.entries.partition_point(|entry| entry.way_id <= way_id);
        let Some(hit) = java_duplicate_key_hit(self.entries.len(), lower, upper) else {
            return Vec::new();
        };
        self.entries[hit..upper]
            .iter()
            .chain(self.entries[lower..hit].iter().rev())
            .collect()
    }
}
fn way_coordinates(
    ids: &[i64],
    mut lookup: impl FnMut(i64) -> Option<(f64, f64)>,
) -> Option<Vec<Coord<f64>>> {
    ids.iter()
        .map(|id| lookup(*id).map(|(lat, lng)| Coord { x: lng, y: lat }))
        .collect()
}

/// Preserve polygon-first dispatch, cloning only when both views are emitted.
fn way_views(mut source: Source, closed: bool) -> [Option<Source>; 2] {
    let Geometry::LineString(line) = &source.geometry else {
        unreachable!()
    };
    let polygon = closed && line.0.len() >= 4 && source.tag("area") != "no";
    let line = source.tag("area") != "yes" || !polygon;
    if !polygon {
        return [None, Some(source)];
    }
    let line_source = line.then(|| source.clone());
    let Geometry::LineString(coords) = source.geometry else {
        unreachable!()
    };
    source.geometry = Geometry::Polygon(Polygon::new(coords, vec![]));
    [Some(source), line_source]
}

/// Source ordinals are `job << 32 | position in the job`. Jobs are numbered in
/// the order the serial reader would have produced their sources, from 1, so
/// ordinals sort exactly like serial emission and stay above the supplement
/// ordinals (plain counters below 2^32).
const JOB_SHIFT: u32 = 32;
/// Relations are queued in runs of about this many members: relation cost
/// tracks size, and a run of small relations should cost about one big one.
const MEMBERS_PER_RELATION_JOB: usize = 256;

/// Everything workers share while assembling OSM sources. It is read-only
/// except for the collectors that ways fill for relation assembly.
pub(super) struct Index {
    prepared: PreparedIngestIndex,
    /// Relation ID to position in `prepared.supported_relations`.
    relations: HashMap<i64, usize>,
    profile_memberships: ProfileMembershipIndex,
    /// Highest-ranked highway touching each motorway junction node.
    junction_classes: Mutex<HashMap<i64, (u8, String)>>,
    /// Node IDs of ways that relations may join, with the ordinal of the way
    /// and, in an area-only compile, the way's extent.
    member_ways: Mutex<HashMap<i64, MemberWay>>,
    missing_ways: AtomicUsize,
    missing_relations: AtomicUsize,
    skipped_ways: AtomicUsize,
    skipped_node_references: AtomicUsize,
    /// Ways of an area-only compile that the node store placed outside the
    /// area without resolving their nodes.
    outside_ways: AtomicUsize,
    /// In an area-only compile, the box whose objects become sources.
    area: Option<InputArea>,
    /// `area` rounded outwards to whole 1e-7 degrees, for the node store.
    area_boxes: Vec<DecimicroBox>,
}

/// A unit of source assembly that runs on a render worker.
pub(super) struct Job {
    index: Arc<Index>,
    pub profile: Arc<Profile>,
    pub ordinal: u64,
    task: Task,
}

enum Task {
    Nodes(NodeBatch),
    /// Motorway junction node IDs seen before this batch.
    Ways(WayBatch, Arc<HashSet<i64>>),
    Sources(Vec<Source>),
    /// Relations `first..end`.
    Relations(usize, usize, Arc<MemberWays>),
}

/// A way relations may join: its ordinal, nodes and extent.
type MemberWay = (u64, Vec<i64>, Extent);

/// The ways relations join, collected while pass 2 reads the ways.
struct MemberWays {
    nodes: HashMap<i64, Vec<i64>>,
    /// Each way's extent, in an area-only compile.
    extents: HashMap<i64, Extent>,
}

/// Tagged nodes per job when an area-only read hands on pass 1's nodes.
const AREA_NODES_PER_BATCH: usize = 8_000;
/// osm-pbf-ingest's desktop default for pass 2's decoded-block queue.
const PASS2_QUEUE_BLOCKS: usize = 16;

/// Share of the read-OSM stage given to the ways of pass 2; the relations
/// assembled after it have the rest.
const PASS2_SHARE: u64 = 850;
const STAGE_UNITS: u64 = 1000;
/// Share of pass 2 given to its node blocks, which go fast. While they are
/// read their count is unknown, so their part only nears it.
const NODES_SHARE: f64 = 0.1;

#[allow(clippy::too_many_arguments)]
pub fn read_osm(
    pbf: &Path,
    temp: &Path,
    workers: usize,
    pipeline: &mut Pipeline,
    profile: &Arc<Profile>,
    control: &Control,
    area: Option<InputArea>,
    progress: &mut dyn FnMut(&str),
    warnings: &mut Vec<String>,
) -> Result<(), String> {
    // The block counts, for the fractions: pass 1 reads the blocks holding
    // nodes or relations, and pass 2 hands on each block's tagged nodes and
    // its ways as one batch, the ways taking most of the time.
    let blobs = osm_pbf_ingest::blob_index::PbfBlobIndex::scan(pbf).ok();
    let count = |kinds: &[ObjectKind]| {
        blobs
            .as_ref()
            .map_or(0, |index| index.blobs_containing_any(kinds).count() as u64)
    };
    let pass1_blobs = count(&[ObjectKind::Node, ObjectKind::Relation]);
    // An area-only read takes the area's tagged nodes from pass 1 and skips
    // the node blobs in pass 2, which then reads these.
    let pass2_area_blobs = count(&[ObjectKind::Way, ObjectKind::Relation]).max(1) as f64;
    // The index cannot tell which blocks hold ways without decoding them, so
    // pass 2 counts them as all blocks but those its nodes came in.
    let all_blobs = blobs.as_ref().map_or(1, |index| index.len()).max(1) as f64;
    let mut node_batches = 0.0;
    drop(blobs);
    control.set_total(pass1_blobs);
    // Declared before the index so that it outlives it: the store is unmapped
    // before its directory goes, which Windows needs.
    let node_store_dir = NodeStoreDir(temp.join(NODE_STORE_DIR));
    let prepare_phase = crate::profiling::Phase::start("omt_prepare_index");
    let index_stage = crate::stage_profile::Stage::start("index_osm");
    let area_boxes = area
        .as_ref()
        .map_or_else(Vec::new, InputArea::decimicro_boxes);
    let mut options = PrepareOptions::new().worker_threads(workers);
    if area.is_some() {
        // Every tagged node near the area; `Index::node_in_area` picks.
        options = options.keep_nodes_within(&area_boxes);
    }
    let mut prepared = prepare_ingest_index_with_options(
        pbf,
        IngestBuildProfile::DesktopFull,
        temp,
        options,
        |report| control.set_done(report.batches_read as u64),
        || control.is_cancelled(),
    )?;
    drop(prepare_phase);
    drop(index_stage);
    let area_nodes = std::mem::take(&mut prepared.tagged_nodes);
    let message = "Reading OSM and preparing vector tiles";
    progress(message);
    control.stage(CompileStage::ReadOsm, message)?;
    control.set_total(STAGE_UNITS);
    let _pass2_phase = crate::profiling::Phase::start("omt_pass2_and_relations");
    let ways_stage = crate::stage_profile::Stage::start("ways");
    let relations = prepared
        .supported_relations
        .iter()
        .enumerate()
        .map(|(index, r)| (r.relation_id, index))
        .collect::<HashMap<_, _>>();
    let profile_memberships = ProfileMembershipIndex::new(
        prepared
            .relation_input_order
            .iter()
            .filter_map(|id| relations.get(id).copied())
            .map(|index| (index, &prepared.supported_relations[index])),
    );
    let index = Arc::new(Index {
        prepared,
        relations,
        profile_memberships,
        junction_classes: Mutex::new(HashMap::new()),
        member_ways: Mutex::new(HashMap::new()),
        missing_ways: AtomicUsize::new(0),
        missing_relations: AtomicUsize::new(0),
        skipped_ways: AtomicUsize::new(0),
        skipped_node_references: AtomicUsize::new(0),
        outside_ways: AtomicUsize::new(0),
        area_boxes,
        area,
    });
    let mut sink = SourceSink {
        pipeline: &mut *pipeline,
        index: index.clone(),
        profile: profile.clone(),
        jobs: 0,
        junctions: HashMap::new(),
        junction_ids: Arc::new(HashSet::new()),
    };
    // The relations come from pass 1; pass 2 would decode them all again
    // for nothing. The render workers are slower than the decode workers, so
    // pass 2's queue of decoded blocks stays full: two blocks per worker,
    // rather than 16 whatever the count, hold fewer of them.
    let mut options = Pass2Options::new()
        .worker_threads(workers)
        .skip_relations()
        .queue_capacity((2 * workers).min(PASS2_QUEUE_BLOCKS));
    if index.area.is_some() {
        // The nodes came from pass 1, in file order as pass 2 would hand them.
        for nodes in area_nodes.chunks(AREA_NODES_PER_BATCH) {
            sink.on_node_batch(NodeBatch {
                nodes: nodes.to_vec(),
                estimated_bytes: 0,
            })?;
        }
        node_batches = all_blobs - pass2_area_blobs;
        let filter = index.clone();
        options = options
            .skip_nodes()
            .keep_ways_where(Arc::new(move |way| !filter.outside_area(way)));
    }
    drop(area_nodes);
    let ways_phase = crate::profiling::Phase::start("omt_pass2_ways");
    run_pass2_with_options(
        pbf,
        IngestBuildProfile::DesktopFull,
        options,
        &mut sink,
        |report| {
            let share = match report.stage {
                IngestStage::IngestPass2Nodes => {
                    node_batches = report.batches_read as f64;
                    NODES_SHARE * node_batches / all_blobs
                }
                IngestStage::IngestPass2Ways => {
                    let way_blobs = (all_blobs - node_batches).max(1.0);
                    NODES_SHARE + (1.0 - NODES_SHARE) * report.batches_read as f64 / way_blobs
                }
                _ => return,
            };
            control.set_done((share.min(1.0) * PASS2_SHARE as f64) as u64);
        },
        || control.is_cancelled(),
    )?;
    control.set_done(PASS2_SHARE);
    sink.finish()?;
    drop(sink);
    drop(ways_phase);
    drop(ways_stage);
    let _relations_phase = crate::profiling::Phase::start("omt_relations");
    let _relations_stage = crate::stage_profile::Stage::start("relations");
    let first = pipeline.processed();
    pipeline.sync_with(|done, sent| {
        let share = (done - first) as f64 / (sent - first).max(1) as f64;
        control.set_done(PASS2_SHARE + (share * (STAGE_UNITS - PASS2_SHARE) as f64) as u64);
    })?;
    #[cfg(feature = "benchmark-profiling")]
    eprintln!(
        "{}",
        serde_json::json!({"benchmark_counts":"omt_early_way_filter",
        "skipped_ways":index.skipped_ways.load(Ordering::Relaxed),
        "outside_ways":index.outside_ways.load(Ordering::Relaxed),
        "skipped_node_references":index.skipped_node_references.load(Ordering::Relaxed)})
    );
    let missing_ways = index.missing_ways.load(Ordering::Relaxed);
    let missing_relations = index.missing_relations.load(Ordering::Relaxed);
    // Every job has finished, so this was the last reference: the node store
    // goes now, not when the compile ends. It is the extract's biggest scratch
    // file (about 2× the extract) and nothing after this stage reads it.
    drop(index);
    drop(node_store_dir);
    if missing_ways > 0 || missing_relations > 0 {
        warnings.push(format!(
            "skipped incomplete source geometries: {missing_ways} ways with nodes missing \
             from the extract, {missing_relations} relations whose rings could not be assembled"
        ));
    }
    Ok(())
}

/// The directory under the scratch directory where `osm_pbf_ingest` keeps a
/// disk node store (`DiskSortedNodeStore::new`). Extracts over 32 MiB get one.
const NODE_STORE_DIR: &str = "node_store";

/// Deletes the disk node store when dropped, on success, error or cancel.
struct NodeStoreDir(std::path::PathBuf);

impl Drop for NodeStoreDir {
    fn drop(&mut self) {
        // Best effort: a store still mapped elsewhere (only on an error path,
        // and only on Windows) stays until the scratch directory is removed.
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// Pass 2 callbacks run on the calling thread. They only forward batches to the
/// render workers, apart from the motorway junction nodes: ways need to know
/// which nodes are junctions, and junctions are emitted after all ways.
struct SourceSink<'a> {
    pipeline: &'a mut Pipeline,
    index: Arc<Index>,
    profile: Arc<Profile>,
    jobs: u64,
    junctions: HashMap<i64, Source>,
    junction_ids: Arc<HashSet<i64>>,
}
impl SourceSink<'_> {
    /// Queues `task` as job number `job`. Jobs are numbered in serial order.
    fn submit(&mut self, job: u64, task: Task) -> Result<(), String> {
        self.pipeline.submit(Job {
            index: self.index.clone(),
            profile: self.profile.clone(),
            ordinal: job << JOB_SHIFT,
            task,
        })
    }

    fn submit_next(&mut self, task: Task) -> Result<(), String> {
        self.jobs += 1;
        self.submit(self.jobs, task)
    }

    /// Waits for pass 2 to finish, then queues the junctions and relations,
    /// which need what the ways collected.
    fn finish(&mut self) -> Result<(), String> {
        self.pipeline.sync()?;
        let mut junctions = self.junctions.drain().collect::<Vec<_>>();
        junctions.sort_by_key(|(id, _)| *id);
        let classes = std::mem::take(&mut *lock(&self.index.junction_classes));
        let junctions = junctions
            .into_iter()
            .map(|(id, mut source)| {
                if let Some((_, highway)) = classes.get(&id) {
                    source
                        .tags
                        .insert("_junction_highway".into(), highway.clone());
                }
                source
            })
            .collect::<Vec<_>>();
        if !junctions.is_empty() {
            self.submit_next(Task::Sources(junctions))?;
        }
        let collected = std::mem::take(&mut *lock(&self.index.member_ways));
        let mut member_ways = MemberWays {
            nodes: HashMap::with_capacity(collected.len()),
            extents: HashMap::new(),
        };
        for (id, (_, nodes, extent)) in collected {
            member_ways.nodes.insert(id, nodes);
            if self.index.area.is_some() {
                member_ways.extents.insert(id, extent);
            }
        }
        let member_ways = Arc::new(member_ways);
        // Jobs are numbered in relation order but queued heaviest first, so a
        // huge relation does not start last and leave the other workers idle.
        let mut runs = Vec::new();
        let (mut first, mut weight) = (0, 0);
        let relations = &self.index.prepared.supported_relations;
        for (position, relation) in relations.iter().enumerate() {
            weight += relation.members.len() + 1;
            if weight >= MEMBERS_PER_RELATION_JOB || position + 1 == relations.len() {
                runs.push((first, position + 1, weight));
                (first, weight) = (position + 1, 0);
            }
        }
        let first_job = self.jobs + 1;
        self.jobs += runs.len() as u64;
        let mut order = (0..runs.len()).collect::<Vec<_>>();
        order.sort_by_key(|&run| std::cmp::Reverse(runs[run].2));
        for run in order {
            let (first, end, _) = runs[run];
            self.submit(
                first_job + run as u64,
                Task::Relations(first, end, member_ways.clone()),
            )?;
        }
        Ok(())
    }
}
impl Pass2Sink for SourceSink<'_> {
    fn on_relation_batch(&mut self, _: RelationBatch) -> Result<(), String> {
        Ok(())
    }
    fn on_node_batch(&mut self, batch: NodeBatch) -> Result<(), String> {
        for n in &batch.nodes {
            if is_junction(&n.tags) && self.index.node_in_area(n) {
                let source = node_source(n)?;
                Arc::make_mut(&mut self.junction_ids).insert(n.node_id);
                self.junctions.insert(n.node_id, source);
            }
        }
        self.submit_next(Task::Nodes(batch))
    }
    fn on_way_batch(&mut self, batch: WayBatch) -> Result<(), String> {
        let junctions = self.junction_ids.clone();
        self.submit_next(Task::Ways(batch, junctions))
    }
}

fn is_junction(tags: &NodeTags) -> bool {
    tags.get("highway") == Some("motorway_junction")
}

fn node_source(n: &NormalizedNode) -> Result<Source, String> {
    Ok(Source {
        id: encoded_id(n.node_id, SourceKind::Node)?,
        osm_id: n.node_id,
        kind: SourceKind::Node,
        source_layer: "osm".into(),
        tags: tags(&n.tags),
        geometry: Geometry::Point(Point::new(n.lng, n.lat)),
        relations: vec![],
    })
}

impl Job {
    /// Assembles the job's sources in serial emission order. `emit` receives
    /// each source with its ordinal.
    pub fn run(
        self,
        emit: &mut dyn FnMut(u64, Source) -> Result<(), String>,
    ) -> Result<(), String> {
        let base = self.ordinal;
        match self.task {
            Task::Nodes(batch) => {
                for (position, n) in batch.nodes.iter().enumerate() {
                    // Junction nodes are emitted by `SourceSink::finish`.
                    if n.tags.is_empty() || is_junction(&n.tags) || !self.index.node_in_area(n) {
                        continue;
                    }
                    emit(base + position as u64, node_source(n)?)?;
                }
                Ok(())
            }
            Task::Ways(batch, junctions) => self.index.ways(base, batch, &junctions, emit),
            Task::Sources(sources) => {
                for (position, source) in sources.into_iter().enumerate() {
                    emit(base + position as u64, source)?;
                }
                Ok(())
            }
            Task::Relations(first, end, member_ways) => {
                let relations = &self.index.prepared.supported_relations[first..end];
                for (position, relation) in relations.iter().enumerate() {
                    if let Some(source) = self.index.relation(relation, &member_ways)? {
                        emit(base + position as u64, source)?;
                    }
                }
                Ok(())
            }
        }
    }
}

impl Index {
    /// Whether some relation in `parents` is a boundary, whose rings then
    /// need the way.
    fn has_boundary_parent(&self, parents: &[i64]) -> bool {
        parents.iter().any(|id| {
            self.relations.get(id).is_some_and(|&r| {
                self.prepared.supported_relations[r]
                    .tags
                    .get("type")
                    .is_some_and(|t| t == "boundary")
            })
        })
    }

    /// In an area-only compile, whether pass 2 can drop `way` before it
    /// reaches `ways`: no relation needs it, and the node store places all
    /// its nodes outside the area, so no source and no junction inside the
    /// area comes from it. Ways that `ways` skips anyway stay, for its tally.
    fn outside_area(&self, way: &WayRef<'_>) -> bool {
        let Some(area) = &self.area else {
            return false;
        };
        let polygon_member = self
            .prepared
            .multipolygon_way_ids
            .contains(way.way_id as u64);
        let parents = self.prepared.way_relation_memberships.get(way.way_id);
        if skip_way_geometry(way.tags.is_empty(), parents.is_empty(), polygon_member)
            || polygon_member
            || self.has_boundary_parent(parents)
            || self.may_touch_area(area, way.node_ids)
        {
            return false;
        }
        self.outside_ways.fetch_add(1, Ordering::Relaxed);
        true
    }

    /// Whether the way may reach the area: false only when the boxes of the
    /// node store's blocks place the nodes it holds outside, or when the
    /// store holds every node and their extent misses the area, the test
    /// `ways` applies. A way with a node the store lacks stays, so that
    /// `ways` counts it as missing just as a full read does.
    fn may_touch_area(&self, area: &InputArea, node_ids: &[i64]) -> bool {
        let store = &self.prepared.node_store;
        if !store.may_touch(node_ids, &self.area_boxes) {
            return false;
        }
        // The block boxes are loose in an extract, whose consecutive node IDs
        // are scattered over the region: for a city they let through about a
        // third of a region's ways. Looking the nodes up costs little next to
        // turning those ways into sources only for `ways` to drop them.
        let mut nodes = store.cursor();
        let mut extent = Extent::EMPTY;
        for &id in node_ids {
            let Some((lat, lng)) = nodes.get(id) else {
                return true;
            };
            if area.contains(lat, lng) {
                return true;
            }
            extent.add(lat, lng);
        }
        area.intersects(&extent)
    }

    fn node_in_area(&self, node: &NormalizedNode) -> bool {
        self.area
            .as_ref()
            .is_none_or(|area| area.contains(node.lat, node.lng))
    }

    /// Whether the bounding box of the member ways found touches the area.
    /// Without an area, every relation does.
    fn relation_in_area(&self, way_ids: &[i64], member_ways: &MemberWays) -> bool {
        let Some(area) = &self.area else {
            return true;
        };
        let mut extent = Extent::EMPTY;
        for member in way_ids.iter().filter_map(|id| member_ways.extents.get(id)) {
            extent.extend(member);
        }
        area.intersects(&extent)
    }

    fn memberships(&self, id: i64) -> Vec<Tags> {
        self.profile_memberships
            .get(id)
            .iter()
            .map(|membership| {
                let relation = &self.prepared.supported_relations[membership.relation];
                let mut t = tags(&relation.tags);
                t.insert("_id".into(), relation.relation_id.to_string());
                t.insert(
                    "_role".into(),
                    relation.members[membership.member].role.to_string(),
                );
                t
            })
            .collect()
    }
    fn coordinates(&self, ids: &[i64]) -> Option<Vec<Coord<f64>>> {
        let mut nodes = self.prepared.node_store.cursor();
        way_coordinates(ids, |id| nodes.get(id))
    }
    fn try_close_ring(&self, ring: &mut [i64]) -> bool {
        let mut nodes = self.prepared.node_store.cursor();
        try_close_ring(ring, |id| nodes.get(id))
    }

    /// One way's sources are `base + 2 * position + view`.
    fn ways(
        &self,
        base: u64,
        batch: WayBatch,
        junctions: &HashSet<i64>,
        emit: &mut dyn FnMut(u64, Source) -> Result<(), String>,
    ) -> Result<(), String> {
        let mut classes = Vec::new();
        let mut members = Vec::new();
        let (mut missing, mut skipped, mut skipped_nodes) = (0, 0, 0);
        for (position, mut way) in batch.ways.into_iter().enumerate() {
            let ordinal = base + 2 * position as u64;
            let polygon_member = self
                .prepared
                .multipolygon_way_ids
                .contains(way.way_id as u64);
            let parents = self.prepared.way_relation_memberships.get(way.way_id);
            // Unlike member ways, these cannot affect rings or junction state.
            // Missing-node diagnostics intentionally cover only relevant ways.
            if skip_way_geometry(way.tags.is_empty(), parents.is_empty(), polygon_member) {
                skipped += 1;
                skipped_nodes += way.node_ids.len();
                continue;
            }
            let member = polygon_member || self.has_boundary_parent(parents);
            let mut nodes = self.prepared.node_store.cursor();
            let Some(coords) = way_coordinates(&way.node_ids, |id| nodes.get(id)) else {
                missing += 1;
                continue;
            };
            if coords.len() < 2 {
                continue;
            }
            let highway = way.tags.get("highway").unwrap_or("");
            let rank = junction_highway_rank(highway);
            if rank > 0 {
                for id in &way.node_ids {
                    if junctions.contains(id) {
                        classes.push((*id, rank, highway.to_string()));
                    }
                }
            }
            let extent = (member || self.area.is_some()).then(|| Extent::of(&coords));
            if member {
                // The relations below need the node IDs, the way nothing more.
                let extent = match self.area {
                    Some(_) => extent.unwrap_or(Extent::EMPTY),
                    None => Extent::EMPTY,
                };
                members.push((
                    way.way_id,
                    ordinal,
                    std::mem::take(&mut way.node_ids),
                    extent,
                ));
            }
            if way.tags.is_empty() && parents.is_empty() {
                continue;
            }
            // Outside the area the way still serves its relations above.
            if let Some(area) = &self.area
                && !area.intersects(&extent.unwrap_or(Extent::EMPTY))
            {
                continue;
            }
            let source = Source {
                id: encoded_id(way.way_id, SourceKind::Way)?,
                osm_id: way.way_id,
                kind: SourceKind::Way,
                source_layer: "osm".into(),
                tags: tags(&way.tags),
                geometry: Geometry::LineString(LineString::new(coords)),
                relations: self.memberships(way.way_id),
            };
            for (view, source) in way_views(source, way.is_closed).into_iter().enumerate() {
                if let Some(source) = source {
                    emit(ordinal + view as u64, source)?;
                }
            }
        }
        self.missing_ways.fetch_add(missing, Ordering::Relaxed);
        self.skipped_ways.fetch_add(skipped, Ordering::Relaxed);
        self.skipped_node_references
            .fetch_add(skipped_nodes, Ordering::Relaxed);
        if !classes.is_empty() {
            // A junction's class is its highest-ranked highway; equal ranks are
            // the same highway value, so arrival order cannot matter.
            let mut all = lock(&self.junction_classes);
            for (id, rank, highway) in classes {
                let value = all.entry(id).or_insert((0, String::new()));
                if rank > value.0 {
                    *value = (rank, highway);
                }
            }
        }
        if !members.is_empty() {
            let mut all = lock(&self.member_ways);
            for (way_id, ordinal, nodes, extent) in members {
                // A repeated way ID keeps the later way, as serial order did.
                if all.get(&way_id).is_none_or(|(old, ..)| *old <= ordinal) {
                    all.insert(way_id, (ordinal, nodes, extent));
                }
            }
        }
        Ok(())
    }

    /// Joins and nests the relation's member rings, if it is an area relation.
    fn relation(
        &self,
        relation: &NormalizedRelation,
        member_ways: &MemberWays,
    ) -> Result<Option<Source>, String> {
        let kind = relation.tags.get("type").unwrap_or("");
        if !matches!(kind, "multipolygon" | "boundary" | "land_area") {
            return Ok(None);
        }
        let way_ids = relation
            .members
            .iter()
            .filter(|m| m.member_type == RelationMemberType::Way)
            .map(|m| m.member_id)
            .collect::<Vec<_>>();
        if way_ids.is_empty() || !self.relation_in_area(&way_ids, member_ways) {
            return Ok(None);
        }
        let segments = relation_segments_in_order(way_ids, &member_ways.nodes);
        let mut complete = true;
        let Some(rings) = connect_ring_candidates(segments, true) else {
            self.missing_relations.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        };
        let mut polygons = Vec::new();
        for mut ring in rings {
            if ring.first() != ring.last() && !self.try_close_ring(&mut ring) {
                continue;
            }
            let Some(coords) = self.coordinates(&ring) else {
                complete = false;
                break;
            };
            let p = Polygon::new(LineString::new(coords), vec![]);
            if p.unsigned_area() > 0.0 {
                polygons.push(p);
            }
        }
        if !complete || polygons.is_empty() {
            self.missing_relations.fetch_add(1, Ordering::Relaxed);
            return Ok(None);
        }
        let geometry = nest_rings(polygons);
        Ok(Some(Source {
            id: encoded_id(relation.relation_id, SourceKind::Relation)?,
            osm_id: relation.relation_id,
            kind: SourceKind::Relation,
            source_layer: "osm".into(),
            tags: tags(&relation.tags),
            geometry,
            relations: vec![],
        }))
    }
}

// OsmReader.computeWorldGeometry walks relation.members() in PBF order and
// keeps only the first occurrence of each way. Join orientation depends on it.
fn relation_segments_in_order(ids: Vec<i64>, ways: &HashMap<i64, Vec<i64>>) -> Vec<Vec<i64>> {
    let mut seen = HashSet::new();
    ids.into_iter()
        .filter(|id| seen.insert(*id))
        .filter_map(|id| ways.get(&id).cloned())
        .collect()
}

// Pinned Planetiler OsmMultipolygon.connectPolygonSegments (Apache-2.0):
// endpoint entries reference shared mutable chains, preserving traversal/start.
#[cfg(test)]
fn connect_rings(segments: Vec<Vec<i64>>) -> Option<Vec<Vec<i64>>> {
    connect_ring_candidates(segments, false)
}

fn connect_ring_candidates(segments: Vec<Vec<i64>>, include_open: bool) -> Option<Vec<Vec<i64>>> {
    use std::{cell::RefCell, rc::Rc};
    type Chain = Rc<RefCell<Vec<i64>>>;
    let mut endpoints = BTreeMap::<i64, Chain>::new();
    let mut complete = Vec::new();
    for ids in segments {
        if ids.len() <= 1 {
            continue;
        }
        let first = ids[0];
        let last = *ids.last()?;
        if first == last {
            if ids.len() >= 4 {
                complete.push(ids);
            }
            continue;
        }
        if let Some(chain) = endpoints.remove(&first) {
            {
                let mut matched = chain.borrow_mut();
                if matched[0] == first {
                    matched.reverse();
                }
                matched.extend_from_slice(&ids[1..]);
            }
            let next = endpoints
                .get(&last)
                .filter(|next| !Rc::ptr_eq(next, &chain))
                .cloned();
            if let Some(next) = next {
                endpoints.remove(&last);
                // Distinct shared identities: borrow next while mutating match,
                // copying nodes as Java does, without retaining an obsolete arena.
                let end = {
                    let tail = next.borrow();
                    let mut matched = chain.borrow_mut();
                    if tail[0] == last {
                        matched.extend_from_slice(&tail[1..]);
                    } else {
                        matched.extend(tail.iter().rev().skip(1).copied());
                    }
                    *matched.last()?
                };
                endpoints.insert(end, chain);
            } else {
                endpoints.insert(last, chain);
            }
        } else if let Some(chain) = endpoints.remove(&last) {
            {
                let mut matched = chain.borrow_mut();
                if matched[0] == last {
                    let mut joined = ids[..ids.len() - 1].to_vec();
                    joined.append(&mut matched);
                    *matched = joined;
                } else {
                    matched.extend(ids.iter().rev().skip(1).copied());
                }
            }
            endpoints.insert(first, chain);
        } else {
            let chain = Rc::new(RefCell::new(ids));
            endpoints.insert(first, Rc::clone(&chain));
            endpoints.insert(last, chain);
        }
    }
    // Sorted endpoint traversal preserves reference order and alias behavior.
    // Planetiler forwards one copy of each open chain (at its first endpoint)
    // to getClosedRings, which may close it using the packed World positions.
    for (endpoint, chain) in endpoints {
        let ring = chain.borrow();
        if ring.len() >= 4
            && (ring.first() == ring.last() || (include_open && ring.first() == Some(&endpoint)))
        {
            complete.push(ring.clone());
        }
    }
    (!complete.is_empty()).then_some(complete)
}

fn try_close_ring(ring: &mut [i64], mut coordinate: impl FnMut(i64) -> Option<(f64, f64)>) -> bool {
    // Pinned OsmMultipolygon.tryClose compares decoded packed World coordinates
    // at a 10-cm threshold and replaces, rather than appends, the final ID.
    const MIN_GAP: f64 = 0.1 / (2.0 * std::f64::consts::PI * 6_378_137.0);
    let Some((&first, &last)) = ring.first().zip(ring.last()) else {
        return false;
    };
    let Some((first_lat, first_lng)) = coordinate(first) else {
        return false;
    };
    let Some((last_lat, last_lng)) = coordinate(last) else {
        return false;
    };
    let project = |lat, lng| {
        let Geometry::Point(point) =
            super::interior_point::projected_osm(&Geometry::Point(Point::new(lng, lat)))
        else {
            unreachable!("projecting a point preserves its geometry type")
        };
        point
    };
    let first_coord = project(first_lat, first_lng);
    let last_coord = project(last_lat, last_lng);
    let dx = first_coord.x() - last_coord.x();
    let dy = first_coord.y() - last_coord.y();
    if (dx * dx + dy * dy).sqrt() <= MIN_GAP {
        *ring.last_mut().expect("nonempty ring") = first;
        true
    } else {
        false
    }
}

fn ring_contains(
    outer: &Polygon<f64>,
    inner: &Polygon<f64>,
    inner_bounds: &Rect<f64>,
    inner_area: f64,
) -> bool {
    let Some(vertex) = inner.exterior().0.first().filter(|_| inner_area > 0.0) else {
        return outer.contains(inner);
    };
    let touches = outer.exterior().lines().any(|edge| {
        edge.start.x.min(edge.end.x) <= inner_bounds.max().x
            && edge.start.x.max(edge.end.x) >= inner_bounds.min().x
            && edge.start.y.min(edge.end.y) <= inner_bounds.max().y
            && edge.start.y.max(edge.end.y) >= inner_bounds.min().y
    });
    if touches {
        outer.contains(inner)
    } else {
        outer.coordinate_position(vertex) == CoordPos::Inside
    }
}

fn nest_rings(rings: Vec<Polygon<f64>>) -> Geometry<f64> {
    // Planetiler groups complete projected rings, not a ring's first vertex.
    // A hole may begin on its shell boundary while its interior is contained.
    let mut rings = rings
        .into_iter()
        .map(|ring| {
            let Geometry::Polygon(projected) =
                super::interior_point::projected_osm(&Geometry::Polygon(ring.clone()))
            else {
                unreachable!("projecting a polygon preserves its geometry type")
            };
            (projected.unsigned_area(), ring, projected)
        })
        .collect::<Vec<_>>();
    rings.sort_by(|a, b| b.0.total_cmp(&a.0));
    let areas = rings.iter().map(|ring| ring.0).collect::<Vec<_>>();
    let (rings, projected): (Vec<_>, Vec<_>) = rings
        .into_iter()
        .map(|(_, ring, projected)| (ring, projected))
        .unzip();
    // `contains` implies bounding-rect containment, so the rect test only skips
    // pairs that cannot nest. When no edge of the candidate parent touches the
    // child's rect, the child lies wholly inside or outside it and one vertex
    // decides; full `contains` (a relate over every parent edge) is for the rest.
    let bounds = projected
        .iter()
        .map(|polygon| polygon.bounding_rect().expect("rings are non-empty"))
        .collect::<Vec<_>>();
    let mut parents = vec![None; rings.len()];
    let mut depths = vec![0usize; rings.len()];
    let mut holes = vec![Vec::new(); rings.len()];
    for i in 1..rings.len() {
        for j in (0..i).rev() {
            let (outer, inner) = (&bounds[j], &bounds[i]);
            if outer.min().x <= inner.min().x
                && outer.min().y <= inner.min().y
                && outer.max().x >= inner.max().x
                && outer.max().y >= inner.max().y
                && ring_contains(&projected[j], &projected[i], inner, areas[i])
            {
                parents[i] = Some(j);
                depths[i] = depths[j] + 1;
                if depths[i] % 2 == 1 {
                    holes[j].push(i);
                }
                break;
            }
        }
    }
    let polygons = (0..rings.len())
        .filter(|i| depths[*i] % 2 == 0)
        .map(|i| {
            Polygon::new(
                rings[i].exterior().clone(),
                holes[i]
                    .iter()
                    .map(|j| rings[*j].exterior().clone())
                    .collect(),
            )
        })
        .collect::<Vec<_>>();
    if polygons.len() == 1 {
        Geometry::Polygon(polygons.into_iter().next().unwrap())
    } else {
        Geometry::MultiPolygon(MultiPolygon(polygons))
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn node_store_dir_deletes_the_disk_store_osm_pbf_ingest_writes() {
        use osm_pbf_ingest::node_store::{
            NodeLocationStoreBuilder, NodeStoreStrategy, NodeWorkerChunk,
        };
        let temp = tempfile::tempdir().unwrap();
        let guard = super::NodeStoreDir(temp.path().join(super::NODE_STORE_DIR));
        let builder = NodeLocationStoreBuilder::new(NodeStoreStrategy::Disk, temp.path()).unwrap();
        let mut chunk = NodeWorkerChunk::default();
        chunk.push(1, 50.06, 19.94);
        builder.push_chunk(chunk).unwrap();
        let store = builder.finish().unwrap();
        assert_eq!(
            store.get(1).map(|(lat, _)| (lat * 100.0).round()),
            Some(5006.0)
        );
        assert!(guard.0.is_dir(), "the store is where NODE_STORE_DIR says");
        drop(store);
        drop(guard);
        assert!(!temp.path().join(super::NODE_STORE_DIR).exists());
        assert_eq!(std::fs::read_dir(temp.path()).unwrap().count(), 0);
    }

    #[test]
    fn relation_19960721_nests_boundary_touching_inner_rings_like_planetiler() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-landcover-relation-19960721.json"
        ))
        .unwrap();
        let polygons = fixture["members"]
            .as_array()
            .unwrap()
            .iter()
            .map(|member| {
                let coordinates = member["way"]["coordinates_lonlat"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .map(|point| Coord {
                        x: point[0].as_f64().unwrap(),
                        y: point[1].as_f64().unwrap(),
                    })
                    .collect::<Vec<_>>();
                Polygon::new(LineString::new(coordinates), vec![])
            })
            .collect();
        let result = nest_rings(polygons);
        let Geometry::MultiPolygon(result) = result else {
            panic!("reference has two assembled components");
        };
        assert_eq!(result.0.len(), 2);
        assert_eq!(
            result
                .0
                .iter()
                .map(|polygon| polygon.interiors().len())
                .collect::<Vec<_>>(),
            vec![9, 0]
        );
    }

    #[test]
    fn tenczynski_relation19578700_matches_pbf_order_java_traversal() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-relation-19578700.json"
        ))
        .unwrap();
        let ids: Vec<i64> = serde_json::from_value(fixture["member_ids"].clone()).unwrap();
        let ways: HashMap<i64, Vec<i64>> = fixture["ways"]
            .as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| {
                (
                    k.parse().unwrap(),
                    serde_json::from_value(v["nodes"].clone()).unwrap(),
                )
            })
            .collect();
        let expected: Vec<i64> = serde_json::from_value(fixture["expected_outer"].clone()).unwrap();
        let rings = connect_rings(relation_segments_in_order(ids, &ways)).unwrap();
        assert_eq!(rings, vec![expected]);
    }

    #[test]
    fn relation_member_sequence_preserves_first_occurrence_not_way_id_order() {
        let ways = HashMap::from([(30, vec![3, 4]), (10, vec![1, 2]), (20, vec![2, 3])]);
        assert_eq!(
            relation_segments_in_order(vec![30, 10, 30, 99, 20, 10], &ways),
            vec![vec![3, 4], vec![1, 2], vec![2, 3]]
        );
    }

    #[test]
    fn newly_closed_indexed_chain_can_be_absorbed_but_preclosed_member_survives() {
        // Pinned endpoint-map behavior retains a newly closed chain in the map:
        // later dangling edges can reopen it. Do not extract these rings early.
        assert!(connect_rings(vec![vec![1, 2], vec![2, 3], vec![3, 1], vec![1, 4]]).is_none());
        assert_eq!(
            connect_rings(vec![vec![1, 2, 3, 1], vec![1, 4]]).unwrap(),
            vec![vec![1, 2, 3, 1]]
        );
        // Degenerate duplicate/reversed edges must not panic or invent a ring.
        assert!(connect_rings(vec![vec![1, 2], vec![1, 2]]).is_none());
        assert!(connect_rings(vec![vec![1, 2], vec![2, 1]]).is_none());
        assert!(connect_rings(vec![vec![1, 1], vec![2, 2, 2]]).is_none());
    }
    #[test]
    fn dolinki_relation19549848_matches_pinned_outer_start_and_traversal() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-relation-19549848.json"
        ))
        .unwrap();
        let segments: Vec<Vec<i64>> = serde_json::from_value(fixture["segments"].clone()).unwrap();
        let expected: Vec<i64> = serde_json::from_value(fixture["expected_outer"].clone()).unwrap();
        let rings = connect_rings(segments).unwrap();
        assert_eq!(rings.len(), 2);
        let outer = rings.iter().find(|r| r.len() == 4155).unwrap();
        assert_eq!(*outer, expected);
        assert_eq!(rings[0].len(), 184); // already-closed inner member precedes joined outer
    }
    #[test]
    fn endpoint_join_reverses_existing_chain_and_bridges_incoming_segments() {
        assert_eq!(
            connect_rings(vec![vec![1, 2, 3], vec![1, 4], vec![4, 3]]).unwrap(),
            vec![vec![3, 2, 1, 4, 3]]
        );
        assert_eq!(
            connect_rings(vec![vec![1, 2], vec![4, 3], vec![2, 3], vec![4, 1]]).unwrap(),
            vec![vec![1, 2, 3, 4, 1]]
        );
        let closed = vec![8, 9, 10, 8];
        assert_eq!(
            connect_rings(vec![closed.clone(), closed.clone(), vec![11, 12]]).unwrap(),
            vec![closed.clone(), closed]
        );
        assert!(connect_rings(vec![vec![], vec![1], vec![2, 3, 4]]).is_none());
    }

    use super::*;
    #[test]
    fn relation_1356150_closes_equal_coordinate_outer_and_keeps_inner_hole() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-relation-1356150-near-close.json"
        ))
        .unwrap();
        let mut ways = HashMap::<i64, Vec<i64>>::new();
        let mut nodes = HashMap::<i64, (f64, f64)>::new();
        for way in fixture["ways"].as_array().unwrap() {
            let id = way["id"].as_i64().unwrap();
            let mut ids = Vec::new();
            for node in way["nodes"].as_array().unwrap() {
                let node_id = node["id"].as_i64().unwrap();
                ids.push(node_id);
                nodes.insert(
                    node_id,
                    (node["lat"].as_f64().unwrap(), node["lon"].as_f64().unwrap()),
                );
            }
            ways.insert(id, ids);
        }
        let outer = ways[&95319986].clone();
        assert_ne!(outer.first(), outer.last());
        assert_eq!(nodes[outer.first().unwrap()], nodes[outer.last().unwrap()]);
        let rings = connect_ring_candidates(
            relation_segments_in_order(vec![95319986, 93277587], &ways),
            true,
        )
        .unwrap();
        assert_eq!(rings.len(), 2);
        let mut polygons = Vec::new();
        for mut ring in rings {
            if ring.first() != ring.last() {
                assert!(try_close_ring(&mut ring, |id| nodes.get(&id).copied()));
            }
            let coordinates = ring
                .iter()
                .map(|id| {
                    let (lat, lng) = nodes[id];
                    Coord { x: lng, y: lat }
                })
                .collect();
            polygons.push(Polygon::new(LineString::new(coordinates), vec![]));
        }
        let Geometry::Polygon(polygon) = nest_rings(polygons) else {
            panic!("outer and inner should form one polygon")
        };
        assert_eq!(polygon.exterior().0.len(), 20);
        assert_eq!(polygon.interiors().len(), 1);
        assert_eq!(polygon.interiors()[0].0.len(), 11);
    }

    #[test]
    fn near_close_uses_packed_world_gap_and_does_not_join_distant_endpoints() {
        let mut exact = vec![1, 2, 3, 4];
        let positions = HashMap::from([
            (1, (50.0, 19.0)),
            (2, (50.0, 19.001)),
            (3, (50.001, 19.001)),
            (4, (50.0, 19.0)),
        ]);
        assert!(try_close_ring(&mut exact, |id| positions.get(&id).copied()));
        assert_eq!(exact, vec![1, 2, 3, 1]);
        let mut far = vec![1, 2, 3, 5];
        let far_positions = HashMap::from([(5, (50.0, 19.00001))]);
        assert!(!try_close_ring(&mut far, |id| {
            positions
                .get(&id)
                .copied()
                .or_else(|| far_positions.get(&id).copied())
        }));
        assert_eq!(far, vec![1, 2, 3, 5]);
        assert!(!try_close_ring(&mut far, |id| positions.get(&id).copied()));
        let candidates =
            connect_ring_candidates(vec![vec![1, 2, 3, 4], vec![6, 7, 8, 6]], true).unwrap();
        assert_eq!(candidates, vec![vec![6, 7, 8, 6], vec![1, 2, 3, 4]]);
        // At the equator two packed-world x units are about 7.5 cm; three
        // are about 11.2 cm. The pinned 10-cm threshold separates them.
        let longitude_unit = 360.0 / 1_073_741_824.0;
        let mut within = vec![1, 2, 3, 4];
        assert!(try_close_ring(&mut within, |id| {
            Some((0.0, if id == 4 { 2.1 * longitude_unit } else { 0.0 }))
        }));
        assert_eq!(within.last(), Some(&1));
        let mut outside = vec![1, 2, 3, 5];
        assert!(!try_close_ring(&mut outside, |id| {
            Some((0.0, if id == 5 { 3.1 * longitude_unit } else { 0.0 }))
        }));
        assert_eq!(outside.last(), Some(&5));
        assert!(connect_ring_candidates(vec![vec![1, 2, 3]], true).is_none());
    }
    #[test]
    fn profile_membership_index_matches_java_duplicate_lookup_order() {
        let relation = |relation_id, way_id, eligible| {
            let mut tags = NodeTags::default();
            if eligible {
                tags.insert("route", "hiking");
                tags.insert("network", "rwn");
            }
            NormalizedRelation {
                relation_id,
                tags,
                members: vec![osm_pbf_ingest::NormalizedRelationMember {
                    member_type: RelationMemberType::Way,
                    member_id: way_id,
                    role: String::new(),
                }],
            }
        };
        let relations = [
            relation(1, 10, true),
            relation(2, 20, true),
            relation(3, 30, true),
            relation(4, 20, true),
            relation(5, 20, true),
            relation(6, 40, true),
            relation(7, 20, false),
        ];
        let index = ProfileMembershipIndex::new(relations.iter().enumerate());
        assert_eq!(index.entries.len(), 6);
        assert_eq!(
            index
                .get(20)
                .iter()
                .map(|entry| relations[entry.relation].relation_id)
                .collect::<Vec<_>>(),
            [4, 5, 2]
        );
        assert!(index.get(99).is_empty());
    }

    #[test]
    fn pbf_route_membership_pivots_match_pinned_java_order() {
        let fixture: serde_json::Value = serde_json::from_str(include_str!(
            "../../tests/fixtures/omt-route-membership-order.json"
        ))
        .unwrap();
        let total = fixture["eligible_way_memberships"].as_u64().unwrap() as usize;
        for row in fixture["targets"].as_array().unwrap() {
            let lower = row["lower"].as_u64().unwrap() as usize;
            let input = row["insertion_order"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_i64().unwrap())
                .collect::<Vec<_>>();
            let hit = java_duplicate_key_hit(total, lower, lower + input.len()).unwrap();
            let offset = hit - lower;
            let got = input[offset..]
                .iter()
                .chain(input[..offset].iter().rev())
                .copied()
                .collect::<Vec<_>>();
            let expected = row["java_get_order"]
                .as_array()
                .unwrap()
                .iter()
                .map(|id| id.as_i64().unwrap())
                .collect::<Vec<_>>();
            assert_eq!(got, expected, "way {}", row["way_id"]);
        }
    }

    #[test]
    fn profile_relation_eligibility_matches_four_pinned_preprocessors() {
        let relation = |pairs: &[(&str, &str)]| {
            let mut tags = NodeTags::default();
            for (key, value) in pairs {
                tags.insert(key, value);
            }
            NormalizedRelation {
                relation_id: 1,
                tags,
                members: vec![],
            }
        };
        assert!(profile_relation_eligible(&relation(&[(
            "type", "building"
        )])));
        assert!(profile_relation_eligible(&relation(&[
            ("waterway", "river"),
            ("name", "Wisła")
        ])));
        assert!(!profile_relation_eligible(&relation(&[(
            "waterway", "river"
        )])));
        assert!(profile_relation_eligible(&relation(&[
            ("type", "boundary"),
            ("boundary", "administrative"),
            ("admin_level", "4.5x")
        ])));
        assert!(!profile_relation_eligible(&relation(&[
            ("type", "boundary"),
            ("boundary", "administrative"),
            ("admin_level", "11")
        ])));
        assert!(profile_relation_eligible(&relation(&[
            ("route", "hiking"),
            ("network", "")
        ])));
        assert!(!profile_relation_eligible(&relation(&[(
            "route", "hiking"
        )])));
        assert_eq!(java_parse_round_int("2.3.4"), None);
        assert_eq!(java_parse_round_int("2..foo"), None);
        assert_eq!(java_parse_round_int("-1.5"), Some(-1));
    }
    #[test]
    fn junction_highway_rank_matches_pinned_exact_highway_classes() {
        for (highway, rank) in [
            ("motorway", 6),
            ("trunk", 5),
            ("primary", 4),
            ("secondary", 3),
            ("tertiary", 2),
            ("unclassified", 1),
        ] {
            assert_eq!(junction_highway_rank(highway), rank);
        }
        for highway in [
            "motorway_link",
            "trunk_link",
            "primary_link",
            "secondary_link",
            "tertiary_link",
            "residential",
            "",
        ] {
            assert_eq!(junction_highway_rank(highway), 0);
        }
        // A primary touching a motorway ramp remains primary at the junction.
        assert!(junction_highway_rank("primary") > junction_highway_rank("motorway_link"));
    }
    #[test]
    fn early_filter_keeps_member_and_tagged_ways() {
        for tags_empty in [false, true] {
            for parents_empty in [false, true] {
                for polygon_member in [false, true] {
                    assert_eq!(
                        skip_way_geometry(tags_empty, parents_empty, polygon_member),
                        (tags_empty, parents_empty, polygon_member) == (true, true, false)
                    );
                }
            }
        }
        // A tagged highway always reaches the existing junction logic; an
        // untagged member still reaches coordinate validation and retention.
        assert!(!skip_way_geometry(false, true, false));
        assert!(!skip_way_geometry(true, false, true));
    }
    #[test]
    fn missing_member_coordinate_rejects_whole_way_without_joining_survivors() {
        let mut visited = vec![];
        let result = way_coordinates(&[1, 2, 3], |id| {
            visited.push(id);
            (id != 2).then_some((50., id as f64))
        });
        assert!(result.is_none());
        assert_eq!(visited, [1, 2]);
        assert_eq!(
            way_coordinates(&[3, 1], |id| Some((50., id as f64))).unwrap(),
            vec![Coord { x: 3., y: 50. }, Coord { x: 1., y: 50. }]
        );
    }
    #[test]
    fn way_views_preserve_eligibility_order_and_owned_metadata() {
        for closed in [false, true] {
            for count in [2, 3, 4, 5] {
                for area in ["", "yes", "no"] {
                    let coords = (0..count)
                        .map(|i| Coord {
                            x: i as f64,
                            y: 50.,
                        })
                        .collect::<Vec<_>>();
                    let source = Source {
                        id: 12,
                        osm_id: 1,
                        kind: SourceKind::Way,
                        source_layer: "osm".into(),
                        tags: BTreeMap::from([("area".into(), area.into())]),
                        geometry: Geometry::LineString(LineString::new(coords.clone())),
                        relations: vec![BTreeMap::from([("_role".into(), "outer".into())])],
                    };
                    let polygon = closed && count >= 4 && area != "no";
                    let line = area != "yes" || !polygon;
                    let views = way_views(source.clone(), closed);
                    assert_eq!(views[0].is_some(), polygon);
                    assert_eq!(views[1].is_some(), line);
                    for view in views.iter().flatten() {
                        assert_eq!(view.id, source.id);
                        assert_eq!(view.tags, source.tags);
                        assert_eq!(view.relations, source.relations);
                    }
                    if let Some(p) = &views[0] {
                        assert_eq!(
                            p.geometry,
                            Geometry::Polygon(Polygon::new(
                                LineString::new(coords.clone()),
                                vec![]
                            ))
                        );
                    }
                    if let Some(l) = &views[1] {
                        assert_eq!(l.geometry, source.geometry);
                    }
                }
            }
        }
    }
    #[test]
    fn ids_preserve_osm_type_namespace() {
        assert_eq!(encoded_id(42, SourceKind::Node).unwrap(), 421);
        assert_eq!(encoded_id(42, SourceKind::Way).unwrap(), 422);
        assert_eq!(encoded_id(42, SourceKind::Relation).unwrap(), 423);
    }
    #[test]
    fn rings_join_by_node_identity_and_reject_missing_members() {
        let rings = connect_rings(vec![vec![1, 2], vec![4, 3], vec![2, 3], vec![4, 1]]).unwrap();
        assert_eq!(rings.len(), 1);
        assert_eq!(rings[0].len(), 5);
        assert_eq!(rings[0].first(), rings[0].last());
        assert!(connect_rings(vec![vec![1, 2], vec![3, 4]]).is_none());
    }
    #[test]
    fn incomplete_chains_do_not_discard_available_closed_rings() {
        let rings = connect_rings(vec![
            vec![9, 1], // shares endpoint with the closed member
            vec![1, 2, 3, 1],
            vec![20], // reference ignores degenerate members
            vec![30, 31],
            vec![31, 32, 30],
            vec![40, 41, 42], // never invent a closing segment
        ])
        .unwrap();
        assert_eq!(rings, vec![vec![1, 2, 3, 1], vec![30, 31, 32, 30]]);
    }
    #[test]
    fn ring_nesting_preserves_holes_and_islands() {
        fn square(a: f64, b: f64) -> Polygon<f64> {
            Polygon::new(
                LineString::from(vec![(a, a), (b, a), (b, b), (a, b), (a, a)]),
                vec![],
            )
        }
        let Geometry::MultiPolygon(p) =
            nest_rings(vec![square(0., 10.), square(2., 8.), square(4., 6.)])
        else {
            panic!("two islands")
        };
        assert_eq!(p.0.len(), 2);
        assert_eq!(p.0[0].interiors().len(), 1);
        assert!((p.unsigned_area() - 68.).abs() < 1e-9);
    }
}
