//! The RAM-first engine (storage format v3).
//!
//! One reader/writer lock owns everything a reader can observe â€” documents,
//! secondary indexes, the lineage watermarks and the change ring â€” so a read
//! never sees a document without the index entry that matches it. Writers stage
//! a batch, durably append it with one `write_all` + one `sync_all`, and only
//! then take the write lock once to publish. Acknowledged writes are therefore
//! always durable, and the fsync is never taken while a reader waits.
//!
//! Failure classes are kept distinct, because collapsing them is how a database
//! silently loses data:
//!
//! - **Transient I/O** (`Interrupted`, Windows `ERROR_SHARING_VIOLATION`) is
//!   retried a bounded number of times. The failed batch is truncated back to
//!   its starting offset first, so a partial write can never be extended twice.
//! - **Uncertain durable tail** â€” a write or fsync that still fails after the
//!   retries â€” poisons the database: nothing is published, and every later
//!   mutation is refused with [`Error::Poisoned`] until the folder is reopened
//!   and recovered. Reads keep working; RAM is correct up to `last_seq`.
//! - **Corruption** is [`Error::Corruption`] naming file, offset and sequence.
//!
//! Index DDL is a schema-only WAL batch that consumes **no** document sequence
//! (herrbasan/nDB#17): a consumer watermark can never park in front of an event
//! that will never appear.
//!
//! Pins are reaped at publish time and when stats are read. That is the only
//! place it can matter: ring entries are evicted only while publishing a write,
//! so an idle database cannot accumulate pressure that a timer would relieve.

use crate::docval::{self, DocRecord, DocVal};
use crate::error::{Error, Result};
use crate::io::Io;
use crate::ram_format::{
    base_file_path, ram_manifest_path, read_ram_base_file, read_ram_manifest, scan_ram_wal,
    truncate_ram_wal_tail, wal_file_name, wal_file_path, write_ram_base_file,
    write_ram_manifest_atomic, RamManifest, RamWalOp, RamWalPatchOp, RamWalRecord, RamWalScan,
    RamWalWriter, RAM_STORAGE_VERSION,
};
use crate::report;
use crate::storage::{self, ChangeBatch, ChangeEvent, ChangeKind, Snapshot, SourceLineage};
use crate::v2_format::{V2IndexKind, V2IndexSpec};
use crate::{
    id, index_keys, ordered_key_bytes, ordered_value, plan_candidates, validate_query_ast,
    ApplyOpsResult, BTreeIndex, DbLock, HashIndex, IndexAccess, KeyBound, OrderedValue, PatchOp,
    QueryOptions, SortDir, INDEX_ENTRY_COST,
};
use parking_lot::{Condvar, Mutex, RwLock};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::ops::Bound;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How much RAM one database may use, and how much change history it keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RamConfig {
    /// Hard ceiling on live documents plus index plus retained ring bytes.
    /// `None` means unbounded.
    pub max_memory_bytes: Option<usize>,
    /// Retained change events.
    pub change_ring_cap_events: usize,
    /// Retained change bytes. Count alone is not a bound: ten 4 MB chat
    /// post-images are 40 MB however few events they are.
    pub change_ring_cap_bytes: usize,
    /// Compact in the background once the active WAL generation grows past
    /// this many bytes. `None` disables it â€” an embedded or unit-test open
    /// never grows a compaction thread behind the caller's back; `ndb serve`
    /// turns it on explicitly via [`RamConfig::server_default`].
    pub auto_compact_wal_bytes: Option<u64>,
}

impl Default for RamConfig {
    fn default() -> Self {
        RamConfig {
            max_memory_bytes: None,
            change_ring_cap_events: 1024,
            change_ring_cap_bytes: 64 * 1024 * 1024,
            auto_compact_wal_bytes: None,
        }
    }
}

impl RamConfig {
    /// The budget `ndb serve` mounts databases with: the embedded defaults plus
    /// WAL-growth-driven background compaction.
    pub fn server_default() -> Self {
        RamConfig {
            auto_compact_wal_bytes: Some(32 * 1024 * 1024),
            ..RamConfig::default()
        }
    }
}

/// Default lifetime of a snapshot pin. Reaping runs on a timer (see
/// `server::DbRegistry`), so an idle server always releases what it pinned.
pub const DEFAULT_PIN_TTL: Duration = Duration::from_secs(300);

/// Live memory, change-feed and stage-timing counters of one RAM-first
/// database. Every field is a snapshot taken under the state lock.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RamStats {
    /// Approximate bytes held by live document trees.
    pub live_doc_bytes: usize,
    /// Approximate bytes held by secondary indexes.
    pub index_bytes: usize,
    /// Approximate bytes held by retained change post-images.
    pub ring_bytes: usize,
    /// Oldest sequence still replayable through the change feed.
    pub oldest_retained_seq: u64,
    /// Cursors refused with `CursorExpired` because their history was evicted.
    pub expired_cursors: u64,
    /// Snapshot pins that have not expired.
    pub active_pins: usize,
    /// Poisoned databases refuse writes until reopen.
    pub poisoned: bool,
    /// Bytes in the active WAL generation.
    pub wal_bytes: u64,
    /// Committed batches written by this process.
    pub batch_count: u64,
    /// Time spent validating queued jobs.
    pub validate_us: u64,
    /// Time jobs spent waiting for the commit leader.
    pub queue_wait_us: u64,
    /// Time spent inside the durable append + fsync.
    pub fsync_us: u64,
    /// Time spent publishing into RAM after the fsync.
    pub publish_us: u64,
}

// â”€â”€â”€ RAM state â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum RamChangeKind {
    Upsert,
    Delete,
}

/// One retained change. `post_image` is the exact published record for `seq`,
/// never a later lookup: serving the feed cannot resurrect a newer state.
#[derive(Clone, Debug)]
struct RamChangeEvent {
    seq: u64,
    epoch: Arc<str>,
    kind: RamChangeKind,
    id: Arc<str>,
    post_image: Option<Arc<DocRecord>>,
    approx_bytes: usize,
}

/// One snapshot pin: the frozen document list plus the ring floor it reserves.
///
/// The list is `Arc`-shared, so pinning a 338 MB database costs one pointer
/// copy â€” never a document clone, and never under the commit lock.
struct Pin {
    cut_seq: u64,
    expires_at: Instant,
    /// The lineage this pin froze; a page answers with it, never with a newer
    /// watermark read from live state.
    lineage: SourceLineage,
    docs: Arc<Vec<Arc<DocRecord>>>,
}

enum RamIndex {
    Hash(HashIndex),
    BTree(BTreeIndex),
}

impl RamIndex {
    fn build(docs: impl Iterator<Item = Arc<DocRecord>>, field: &str, kind: V2IndexKind) -> Self {
        let mut index = match kind {
            V2IndexKind::Hash => RamIndex::Hash(HashIndex::new()),
            V2IndexKind::BTree => RamIndex::BTree(BTreeIndex::new()),
        };
        for record in docs {
            let keys = field_keys(&record.doc, field);
            if !keys.is_empty() {
                index.insert_keys(&keys, record.id.as_ref());
            }
        }
        index
    }

    fn kind(&self) -> V2IndexKind {
        match self {
            RamIndex::Hash(_) => V2IndexKind::Hash,
            RamIndex::BTree(_) => V2IndexKind::BTree,
        }
    }

    fn insert_keys(&mut self, keys: &[OrderedValue], doc_id: &str) {
        match self {
            RamIndex::Hash(index) => index.insert_keys(keys, doc_id),
            RamIndex::BTree(index) => index.insert_keys(keys, doc_id),
        }
    }

    fn remove_keys(&mut self, keys: &[OrderedValue], doc_id: &str) {
        match self {
            RamIndex::Hash(index) => index.remove_keys(keys, doc_id),
            RamIndex::BTree(index) => index.remove_keys(keys, doc_id),
        }
    }

    fn lookup(&self, keys: &[OrderedValue]) -> Vec<String> {
        match self {
            RamIndex::Hash(index) => index.lookup_keys(keys),
            RamIndex::BTree(index) => index.lookup_keys(keys),
        }
    }

    fn range(&self, min: Bound<OrderedValue>, max: Bound<OrderedValue>) -> Option<Vec<String>> {
        match self {
            RamIndex::Hash(_) => None,
            RamIndex::BTree(index) => Some(index.range_keys(min, max)),
        }
    }

    fn approx_bytes(&self) -> usize {
        match self {
            RamIndex::Hash(index) => index.approx_bytes(),
            RamIndex::BTree(index) => index.approx_bytes(),
        }
    }
}

/// The single visibility boundary.
struct RamState {
    docs: HashMap<Arc<str>, Arc<DocRecord>>,
    deleted: HashSet<Arc<str>>,
    indexes: HashMap<String, RamIndex>,
    lineage: SourceLineage,
    /// The lineage epoch every retained change belongs to.
    epoch: Arc<str>,
    manifest: RamManifest,
    change_ring: VecDeque<RamChangeEvent>,
    ring_bytes: usize,
    expired_cursors: u64,
    live_doc_bytes: usize,
    index_bytes: usize,
    pins: HashMap<u64, Pin>,
    next_pin: u64,
    min_pinned_seq: Option<u64>,
}

/// The keys one document contributes to one indexed field, deduplicated so the
/// byte accounting counts each `(key, id)` pair exactly once.
fn field_keys(doc: &DocVal, field: &str) -> Vec<OrderedValue> {
    let mut keys = doc.extract_ordered_keys(field);
    keys.sort();
    keys.dedup();
    keys
}

fn keys_bytes(keys: &[OrderedValue], doc_id: &str) -> usize {
    keys.iter()
        .map(|key| ordered_key_bytes(key) + doc_id.len() + INDEX_ENTRY_COST)
        .sum()
}

impl RamState {
    fn live_count(&self) -> usize {
        self.docs.len()
    }

    fn doc(&self, id: &str) -> Option<&Arc<DocRecord>> {
        self.docs.get(id)
    }

    /// Index one document into every declared index and count the bytes.
    fn index_doc(&mut self, record: &Arc<DocRecord>) {
        let mut added = 0usize;
        for (field, index) in self.indexes.iter_mut() {
            let keys = field_keys(&record.doc, field);
            if keys.is_empty() {
                continue;
            }
            added += keys_bytes(&keys, record.id.as_ref());
            index.insert_keys(&keys, record.id.as_ref());
        }
        self.index_bytes += added;
    }

    fn unindex_doc(&mut self, record: &Arc<DocRecord>) {
        let mut removed = 0usize;
        for (field, index) in self.indexes.iter_mut() {
            let keys = field_keys(&record.doc, field);
            if keys.is_empty() {
                continue;
            }
            removed += keys_bytes(&keys, record.id.as_ref());
            index.remove_keys(&keys, record.id.as_ref());
        }
        self.index_bytes = self.index_bytes.saturating_sub(removed);
    }

    fn push_event(&mut self, event: RamChangeEvent, config: &RamConfig) {
        self.ring_bytes += event.approx_bytes;
        self.change_ring.push_back(event);
        self.enforce_ring_caps(config);
    }

    /// Evict unpinned history, oldest first, until both caps hold.
    ///
    /// A pinned entry blocks eviction rather than being dropped: a pin exists
    /// precisely so its post-images stay readable.
    fn enforce_ring_caps(&mut self, config: &RamConfig) {
        while self.change_ring.len() > config.change_ring_cap_events
            || self.ring_bytes > config.change_ring_cap_bytes
        {
            let pinned = match self.change_ring.front() {
                None => break,
                Some(event) => self.min_pinned_seq.map_or(false, |floor| event.seq >= floor),
            };
            if pinned {
                break;
            }
            if let Some(event) = self.change_ring.pop_front() {
                self.ring_bytes = self.ring_bytes.saturating_sub(event.approx_bytes);
            }
        }
        self.refresh_available_seq();
    }

    fn refresh_available_seq(&mut self) {
        let floor = self
            .change_ring
            .front()
            .map(|event| event.seq)
            .unwrap_or_else(|| self.lineage.last_seq.saturating_add(1));
        self.lineage.min_available_seq = self.lineage.min_available_seq.max(floor);
    }

    fn recompute_min_pinned(&mut self) {
        self.min_pinned_seq = self.pins.values().map(|pin| pin.cut_seq).min();
    }

    fn memory_used(&self) -> usize {
        self.live_doc_bytes + self.index_bytes + self.ring_bytes
    }

    fn reap_expired_pins_locked(&mut self) -> usize {
        let now = Instant::now();
        let before = self.pins.len();
        self.pins.retain(|_, pin| pin.expires_at > now);
        let reaped = before - self.pins.len();
        if reaped > 0 {
            self.recompute_min_pinned();
        }
        reaped
    }
}

// â”€â”€â”€ Group-commit plumbing â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// What a committed job returns to its caller.
enum Reply {
    Id(String),
    Applied(bool),
    Ops(ApplyOpsResult),
    Unit,
}

impl Reply {
    fn label(&self) -> &'static str {
        match self {
            Reply::Id(_) => "Id",
            Reply::Applied(_) => "Applied",
            Reply::Ops(_) => "Ops",
            Reply::Unit => "Unit",
        }
    }
}

/// One queued mutation. The job carries the *intent*; the read-modify-write
/// runs under the commit leader so concurrent writers cannot interleave.
enum Job {
    Insert {
        doc: Value,
        prefix: Option<String>,
    },
    Update {
        id: String,
        doc: Value,
        if_seq: Option<u64>,
    },
    Set {
        id: String,
        path: String,
        value: Value,
    },
    Remove {
        id: String,
        path: String,
    },
    Push {
        id: String,
        field: String,
        value: Value,
    },
    ApplyOps {
        id: String,
        ops: Vec<PatchOp>,
        if_seq: Option<u64>,
    },
    Delete {
        id: String,
    },
    Restore {
        id: String,
        doc: Value,
    },
}

struct QueuedJob {
    id: u64,
    enqueued: Instant,
    job: Job,
    reply: Sender<Result<Reply>>,
}

struct QueueState {
    queue: VecDeque<QueuedJob>,
    leader_active: bool,
}

/// Fixed bookkeeping overhead of a change-ring entry that carries a post-image.
const EVENT_OVERHEAD: usize = 128;

/// Fixed bookkeeping overhead of a delete event (no post-image).
const DELETE_EVENT_OVERHEAD: usize = 64;

/// A patch copies only the nodes on the mutated path; everything else stays
/// structurally shared with `state.docs`. Its ring entry therefore retains the
/// divergence â€” bounded by the encoded op plus the copied ancestors â€” not a
/// whole document image.
fn patch_event_bytes(record: &DocRecord, wal: &RamWalRecord) -> usize {
    record.id.len() + EVENT_OVERHEAD + wal.payload_bytes() * 2 + 8192
}

/// One staged RAM mutation, applied only after the batch fsync succeeded.
///
/// `ring_bytes` is the memory this change-ring entry will retain beyond what
/// `state.docs` already holds â€” decided while staging, so admission checks and
/// published `ring_bytes` use the same number.
enum PubOp {
    Put {
        record: Arc<DocRecord>,
        ring_bytes: usize,
    },
    Delete {
        id: Arc<str>,
        seq: u64,
        ring_bytes: usize,
    },
}

/// The staged view of one batch: an overlay of not-yet-committed documents, the
/// WAL records they produce, and the RAM mutations waiting for the fsync.
struct Stage {
    overlay: HashMap<String, Option<Arc<DocRecord>>>,
    records: Vec<RamWalRecord>,
    pubs: Vec<PubOp>,
    next_seq: u64,
    live_delta: isize,
    ring_add_bytes: usize,
}

impl Stage {
    fn new(start_seq: u64) -> Self {
        Stage {
            overlay: HashMap::new(),
            records: Vec::new(),
            pubs: Vec::new(),
            next_seq: start_seq,
            live_delta: 0,
            ring_add_bytes: 0,
        }
    }

    fn current(&self, state: &RamState, id: &str) -> Option<Arc<DocRecord>> {
        match self.overlay.get(id) {
            Some(entry) => entry.clone(),
            None => state.doc(id).cloned(),
        }
    }

    fn doc_seq_of(&self, state: &RamState, id: &str) -> u64 {
        self.current(state, id).map_or(0, |record| record.doc_seq)
    }

    fn take_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    fn stage_put(
        &mut self,
        state: &RamState,
        record: Arc<DocRecord>,
        wal: RamWalRecord,
        ring_bytes: usize,
    ) {
        if let Some(previous) = self.current(state, record.id.as_ref()) {
            self.live_delta -= previous.approx_bytes as isize;
        }
        self.live_delta += record.approx_bytes as isize;
        self.ring_add_bytes += ring_bytes;
        self.records.push(wal);
        self.overlay
            .insert(record.id.to_string(), Some(Arc::clone(&record)));
        self.pubs.push(PubOp::Put { record, ring_bytes });
    }

    /// Run one job against the current state plus everything staged before it.
    fn run(&mut self, state: &RamState, job: &Job) -> Result<Reply> {
        match job {
            Job::Insert { doc, prefix } => {
                let overlay = &self.overlay;
                let check = |candidate: &str| {
                    overlay.contains_key(candidate)
                        || state.docs.contains_key(candidate)
                        || state.deleted.contains(candidate)
                };
                let new_id = match prefix {
                    Some(prefix) => id::generate_checked_with_prefix(prefix, check),
                    None => id::generate_checked(check),
                };
                let mut doc = doc.clone();
                let object = doc
                    .as_object_mut()
                    .ok_or_else(|| Error::invalid_arg("document must be a JSON object"))?;
                object.insert("_id".to_string(), Value::String(new_id.clone()));
                object.remove(storage::SEQ_FIELD);
                let seq = self.take_seq();
                let json = serde_json::to_vec(&doc)?;
                let record =
                    DocRecord::new(Arc::from(new_id.as_str()), seq, DocVal::from_value(&doc));
                let wal = RamWalRecord::insert(seq, new_id.clone(), json);
                let ring_bytes = record.id.len() + EVENT_OVERHEAD;
                self.stage_put(state, record, wal, ring_bytes);
                Ok(Reply::Id(new_id))
            }
            Job::Update {
                id: doc_id,
                doc,
                if_seq,
            } => {
                let old = self
                    .current(state, doc_id)
                    .ok_or_else(|| Error::not_found(doc_id))?;
                if let Some(expected) = if_seq {
                    let current = self.doc_seq_of(state, doc_id);
                    if *expected != current {
                        return Err(Error::Conflict {
                            current_seq: current,
                            doc: old.doc.to_value(),
                        });
                    }
                }
                let mut doc = doc.clone();
                let object = doc
                    .as_object_mut()
                    .ok_or_else(|| Error::invalid_arg("document must be a JSON object"))?;
                object.insert("_id".to_string(), Value::String(doc_id.clone()));
                object.remove(storage::SEQ_FIELD);
                let seq = self.take_seq();
                let json = serde_json::to_vec(&doc)?;
                let record =
                    DocRecord::new(Arc::from(doc_id.as_str()), seq, DocVal::from_value(&doc));
                let wal = RamWalRecord::replace(seq, doc_id.clone(), json);
                // The displaced root leaves `live_doc_bytes` but earlier ring
                // entries may still hold it.
                let ring_bytes = record.id.len() + EVENT_OVERHEAD + old.approx_bytes;
                self.stage_put(state, record, wal, ring_bytes);
                Ok(Reply::Unit)
            }
            Job::Set {
                id: doc_id,
                path,
                value,
            } => {
                let old = self
                    .current(state, doc_id)
                    .ok_or_else(|| Error::not_found(doc_id))?;
                let tree = DocVal::from_value(value);
                let (next, applied, delta) = docval::set_path(&old.doc, path, tree)?;
                if !applied {
                    return Ok(Reply::Applied(false));
                }
                let seq = self.take_seq();
                let record = DocRecord::with_delta(&old, seq, next, delta);
                let value_json = serde_json::to_vec(value)?;
                let wal = RamWalRecord::patch(
                    seq,
                    doc_id.clone(),
                    vec![RamWalPatchOp::set(path, value_json)],
                );
                let ring_bytes = patch_event_bytes(&record, &wal);
                self.stage_put(state, record, wal, ring_bytes);
                Ok(Reply::Applied(true))
            }
            Job::Remove { id: doc_id, path } => {
                let old = self
                    .current(state, doc_id)
                    .ok_or_else(|| Error::not_found(doc_id))?;
                let (next, applied, delta) = docval::remove_path(&old.doc, path)?;
                if !applied {
                    return Ok(Reply::Applied(false));
                }
                let seq = self.take_seq();
                let record = DocRecord::with_delta(&old, seq, next, delta);
                let wal = RamWalRecord::patch(seq, doc_id.clone(), vec![RamWalPatchOp::remove(path)]);
                let ring_bytes = patch_event_bytes(&record, &wal);
                self.stage_put(state, record, wal, ring_bytes);
                Ok(Reply::Applied(true))
            }
            Job::Push {
                id: doc_id,
                field,
                value,
            } => {
                let old = self
                    .current(state, doc_id)
                    .ok_or_else(|| Error::not_found(doc_id))?;
                let tree = DocVal::from_value(value);
                let (next, applied, delta) = docval::array_push_path(&old.doc, field, tree)?;
                if !applied {
                    return Ok(Reply::Applied(false));
                }
                let seq = self.take_seq();
                let record = DocRecord::with_delta(&old, seq, next, delta);
                let value_json = serde_json::to_vec(value)?;
                let wal = RamWalRecord::patch(
                    seq,
                    doc_id.clone(),
                    vec![RamWalPatchOp::array_push(field, value_json)],
                );
                let ring_bytes = patch_event_bytes(&record, &wal);
                self.stage_put(state, record, wal, ring_bytes);
                Ok(Reply::Applied(true))
            }
            Job::ApplyOps {
                id: doc_id,
                ops,
                if_seq,
            } => {
                let old = self
                    .current(state, doc_id)
                    .ok_or_else(|| Error::not_found(doc_id))?;
                if let Some(expected) = if_seq {
                    let current = self.doc_seq_of(state, doc_id);
                    if *expected != current {
                        return Err(Error::Conflict {
                            current_seq: current,
                            doc: old.doc.to_value(),
                        });
                    }
                }
                let mut tree = Arc::clone(&old.doc);
                let mut total_delta = 0isize;
                let mut wal_ops = Vec::with_capacity(ops.len());
                let mut applied = Vec::with_capacity(ops.len());
                for (index, op) in ops.iter().enumerate() {
                    let (next, ok, delta, wal_op) = match op {
                        PatchOp::Set { path, value } => {
                            let value_tree = DocVal::from_value(value);
                            let (next, ok, delta) = docval::set_path(&tree, path, value_tree)?;
                            (next, ok, delta, RamWalPatchOp::set(path, serde_json::to_vec(value)?))
                        }
                        PatchOp::Remove { path } => {
                            let (next, ok, delta) = docval::remove_path(&tree, path)?;
                            (next, ok, delta, RamWalPatchOp::remove(path))
                        }
                        PatchOp::ArrayPush { path, value } => {
                            let value_tree = DocVal::from_value(value);
                            let (next, ok, delta) =
                                docval::array_push_path(&tree, path, value_tree)?;
                            (
                                next,
                                ok,
                                delta,
                                RamWalPatchOp::array_push(path, serde_json::to_vec(value)?),
                            )
                        }
                    };
                    if !ok {
                        return Err(Error::invalid_arg(format!(
                            "op {index} ({}) did not resolve; no changes were applied",
                            match op {
                                PatchOp::Set { path, .. } => format!("set '{path}'"),
                                PatchOp::Remove { path } => format!("remove '{path}'"),
                                PatchOp::ArrayPush { path, .. } => format!("array_push '{path}'"),
                            }
                        )));
                    }
                    tree = next;
                    total_delta += delta;
                    wal_ops.push(wal_op);
                    applied.push(true);
                }
                let seq = self.take_seq();
                let record = DocRecord::with_delta(&old, seq, Arc::clone(&tree), total_delta);
                let doc_value = tree.to_value();
                let wal = RamWalRecord::patch(seq, doc_id.clone(), wal_ops);
                let ring_bytes = patch_event_bytes(&record, &wal);
                self.stage_put(state, record, wal, ring_bytes);
                Ok(Reply::Ops(ApplyOpsResult {
                    applied,
                    seq,
                    doc: doc_value,
                }))
            }
            Job::Delete { id: doc_id } => {
                let old = self
                    .current(state, doc_id)
                    .ok_or_else(|| Error::not_found(doc_id))?;
                let seq = self.take_seq();
                self.live_delta -= old.approx_bytes as isize;
                // The root leaves `live_doc_bytes`; earlier ring entries may
                // still hold it, so the delete event carries it too.
                let ring_bytes = old.id.len() + DELETE_EVENT_OVERHEAD + old.approx_bytes;
                self.ring_add_bytes += ring_bytes;
                self.records.push(RamWalRecord::delete(seq, doc_id.clone()));
                self.overlay.insert(doc_id.clone(), None);
                self.pubs.push(PubOp::Delete {
                    id: Arc::from(doc_id.as_str()),
                    seq,
                    ring_bytes,
                });
                Ok(Reply::Unit)
            }
            Job::Restore { id: doc_id, doc } => {
                let mut doc = doc.clone();
                let object = doc
                    .as_object_mut()
                    .ok_or_else(|| Error::invalid_arg("document must be a JSON object"))?;
                object.insert("_id".to_string(), Value::String(doc_id.clone()));
                object.remove(storage::SEQ_FIELD);
                let seq = self.take_seq();
                let json = serde_json::to_vec(&doc)?;
                let record =
                    DocRecord::new(Arc::from(doc_id.as_str()), seq, DocVal::from_value(&doc));
                let wal = RamWalRecord::restore(seq, doc_id.clone(), json);
                let ring_bytes = record.id.len() + EVENT_OVERHEAD;
                self.stage_put(state, record, wal, ring_bytes);
                Ok(Reply::Unit)
            }
        }
    }
}

/// Wall-clock spent in each stage, cumulative for the process.
#[derive(Debug, Default)]
struct StageTimings {
    validate_us: u64,
    queue_wait_us: u64,
    fsync_us: u64,
    publish_us: u64,
    batch_count: u64,
}

// â”€â”€â”€ The engine â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// The live RAM-first backend of one `Database`.
pub(crate) struct RamEngine {
    dir: PathBuf,
    config: RamConfig,
    io: Arc<dyn Io>,
    state: RwLock<RamState>,
    wal: Mutex<RamWalWriter>,
    commit_lock: Mutex<()>,
    /// Serialises compactions. Never held by a writer, so a compaction in
    /// flight cannot block a commit.
    compaction_lock: Mutex<()>,
    queue: Mutex<QueueState>,
    queue_cv: Condvar,
    next_job: AtomicU64,
    poisoned: AtomicBool,
    poison_reason: Mutex<Option<String>>,
    timings: Mutex<StageTimings>,
    auto_compact: Mutex<Option<AutoCompact>>,
    _lock: DbLock,
}

/// The stop signal and handle of the WAL-growth compaction thread.
struct AutoCompact {
    stop: Arc<AutoCompactStop>,
    handle: Mutex<Option<std::thread::JoinHandle<()>>>,
}

struct AutoCompactStop {
    flag: Mutex<bool>,
    wake: Condvar,
}

/// Bounded retries for a transient durable-write failure.
const DURABLE_RETRIES: usize = 3;
/// How often the compaction thread looks at the WAL size.
const AUTO_COMPACT_POLL: Duration = Duration::from_millis(500);

/// Upper bound on the pre-sized serializer buffer. `approx_bytes` counts heap
/// nodes, not JSON characters, so it over-estimates; a huge document must not
/// reserve its whole estimate up front.
const RAW_JSON_MAX_CAPACITY: usize = 8 * 1024 * 1024;

/// Initial capacity for one document's JSON buffer.
///
/// `approx_bytes` is a safe upper bound on the encoded length (it counts the
/// per-node `Arc`/`BTreeMap` overhead JSON omits), so reserving it means a
/// multi-megabyte conversation serializes without a single reallocation.
fn raw_json_capacity(approx_bytes: usize) -> usize {
    approx_bytes.min(RAW_JSON_MAX_CAPACITY)
}

impl std::fmt::Debug for RamEngine {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RamEngine")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

fn transient_io(error: &Error) -> bool {
    match error {
        Error::Io { source, .. } => {
            source.kind() == std::io::ErrorKind::Interrupted || source.raw_os_error() == Some(32)
        }
        _ => false,
    }
}

impl RamEngine {
    /// Open (or initialise) a v3 database folder.
    ///
    /// The caller already holds the OS writer lock, so two processes can never
    /// both reach creation or append.
    pub(crate) fn open_with_io(
        dir: &Path,
        config: RamConfig,
        io: Arc<dyn Io>,
        lock: DbLock,
    ) -> Result<RamEngine> {
        std::fs::create_dir_all(dir).map_err(Error::io_err(dir, "create ram database directory"))?;

        let manifest_path = ram_manifest_path(dir);
        let manifest = if manifest_path.exists() {
            read_ram_manifest(dir)?
        } else {
            initialise(dir, &io)?
        };

        // â”€â”€ Base â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        let base_path = base_file_path(dir, manifest.base_gen);
        let base = read_ram_base_file(&base_path)?;
        if base.base_seq > manifest.last_seq {
            return Err(Error::corruption(
                &base_path,
                format!(
                    "base is at seq {} but the manifest declares last_seq {}",
                    base.base_seq, manifest.last_seq
                ),
            ));
        }

        let mut docs: HashMap<Arc<str>, Arc<DocRecord>> = HashMap::with_capacity(base.docs.len());
        let mut live_doc_bytes = 0usize;
        for record in base.docs {
            live_doc_bytes += record.approx_bytes;
            docs.insert(Arc::clone(&record.id), record);
        }

        // â”€â”€ WAL scan (primary + a stalled compaction's target) â”€â”€â”€â”€â”€â”€
        let primary_path = wal_file_path(dir, manifest.wal_gen);
        let mut scans: Vec<(PathBuf, RamWalScan)> =
            vec![(primary_path.clone(), scan_ram_wal(&primary_path)?)];
        if let Some(pending_gen) = manifest.pending_wal_gen {
            let pending_path = wal_file_path(dir, pending_gen);
            scans.push((pending_path.clone(), scan_ram_wal(&pending_path)?));
        }

        // A torn tail is an interrupted append, not committed data: clear it
        // (with a warning) *before* any writer can open the file.
        for (path, scan) in &scans {
            if let Some(torn) = scan.torn_tail_offset {
                report::warning(
                    "ram open",
                    format!(
                        "{}: truncating a torn WAL tail ({} byte(s)) before recovery",
                        path.display(),
                        scan.total_bytes - torn
                    ),
                );
                truncate_ram_wal_tail(path, torn, io.as_ref())?;
            }
        }

        // â”€â”€ Replay: committed batches ascending, deduped by seq â”€â”€â”€â”€â”€
        let mut doc_records: BTreeMap<u64, RamWalRecord> = BTreeMap::new();
        let mut schema_records: Vec<RamWalRecord> = Vec::new();
        let mut pending_present = false;
        for (generation, (_, scan)) in scans.iter().enumerate() {
            pending_present |= generation > 0;
            for batch in &scan.batches {
                for record in &batch.records {
                    if record.op.consumes_seq() {
                        if record.seq <= base.base_seq {
                            // Folded into the base already.
                            continue;
                        }
                        doc_records.insert(record.seq, record.clone());
                    } else {
                        schema_records.push(record.clone());
                    }
                }
            }
        }

        // Continuous sequence, no hole between the base and the last batch.
        let mut expected = base.base_seq + 1;
        for seq in doc_records.keys() {
            if *seq != expected {
                return Err(Error::corruption(
                    &primary_path,
                    format!("recovery is missing seq {expected}: the next intact record is {seq}"),
                ));
            }
            expected += 1;
        }
        let lineage_seq = expected - 1;
        if lineage_seq < manifest.last_seq {
            return Err(Error::corruption(
                &manifest_path,
                format!(
                    "recovery reached only seq {lineage_seq}, the manifest declares last_seq {}",
                    manifest.last_seq
                ),
            ));
        }

        let mut deleted: HashSet<Arc<str>> = HashSet::new();
        let mut replay_events: Vec<RamChangeEvent> = Vec::new();
        let state_epoch: Arc<str> = Arc::from(manifest.source_epoch_uuid.as_str());
        for (seq, record) in &doc_records {
            let record_id = record_id(&record.op);
            let previous = docs.get(record_id.as_str()).cloned();
            apply_record(&mut docs, &mut deleted, &mut live_doc_bytes, *seq, record)?;
            let post = docs.get(record_id.as_str()).cloned();
            let (kind, id, approx_bytes) = match &record.op {
                RamWalOp::Insert { .. } | RamWalOp::Restore { .. } => {
                    let post = post_after_replay(post.as_ref(), *seq, &record_id)?;
                    (
                        RamChangeKind::Upsert,
                        Arc::clone(&post.id),
                        post.id.len() + EVENT_OVERHEAD,
                    )
                }
                RamWalOp::Replace { .. } => {
                    let post = post_after_replay(post.as_ref(), *seq, &record_id)?;
                    let displaced = previous.as_ref().map_or(0, |old| old.approx_bytes);
                    (
                        RamChangeKind::Upsert,
                        Arc::clone(&post.id),
                        post.id.len() + EVENT_OVERHEAD + displaced,
                    )
                }
                RamWalOp::Patch { .. } => {
                    let post = post_after_replay(post.as_ref(), *seq, &record_id)?;
                    (
                        RamChangeKind::Upsert,
                        Arc::clone(&post.id),
                        patch_event_bytes(post, record),
                    )
                }
                RamWalOp::Delete { .. } => {
                    let retained = previous.as_ref().map_or(0, |old| old.approx_bytes);
                    let id: Arc<str> = Arc::from(record_id.as_str());
                    let approx_bytes = id.len() + DELETE_EVENT_OVERHEAD + retained;
                    (RamChangeKind::Delete, id, approx_bytes)
                }
                RamWalOp::IndexAdd { .. } | RamWalOp::IndexDrop { .. } => {
                    return Err(Error::corruption(
                        &primary_path,
                        format!("schema record at document seq {seq} cannot occupy a slot"),
                    ));
                }
            };
            replay_events.push(RamChangeEvent {
                seq: *seq,
                epoch: Arc::clone(&state_epoch),
                kind,
                id,
                approx_bytes,
                post_image: if kind == RamChangeKind::Upsert { post } else { None },
            });
        }

        // â”€â”€ Open the writer, then fold a pending generation into it â”€
        let mut writer =
            RamWalWriter::create_or_open(&primary_path, manifest.wal_gen, Arc::clone(&io))?;
        let mut manifest = manifest;

        if pending_present {
            // Re-append the adopted records to the active generation so they
            // are durable under a generation the next boot will scan, then the
            // stalled file can be removed.
            for batch in scans[1].1.batches.clone() {
                let mut kept = Vec::new();
                for record in batch.records {
                    if !record.op.consumes_seq() || record.seq > writer.last_seq() {
                        kept.push(record);
                    }
                }
                if kept.is_empty() {
                    continue;
                }
                writer.append_batch_and_sync(&kept)?;
            }
        }

        // Schema operations from any generation are adopted by the manifest.
        for record in &schema_records {
            match &record.op {
                RamWalOp::IndexAdd { field, kind } => {
                    if !manifest.indexes.iter().any(|spec| spec.field == *field) {
                        manifest.indexes.push(V2IndexSpec {
                            field: field.clone(),
                            kind: *kind,
                        });
                    }
                }
                RamWalOp::IndexDrop { field } => {
                    manifest.indexes.retain(|spec| spec.field != *field);
                }
                _ => {}
            }
        }

        // â”€â”€ Indexes from the final document set â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        let mut indexes: HashMap<String, RamIndex> = HashMap::new();
        let mut index_bytes = 0usize;
        for spec in &manifest.indexes {
            let built = RamIndex::build(docs.values().cloned(), &spec.field, spec.kind);
            index_bytes += built.approx_bytes();
            indexes.insert(spec.field.clone(), built);
        }

        // â”€â”€ Change ring retention â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        let mut state = RamState {
            docs,
            deleted,
            indexes,
            lineage: SourceLineage {
                source_epoch_uuid: manifest.source_epoch_uuid.clone(),                last_seq: lineage_seq,
                min_available_seq: manifest.min_available_seq,
            },
            manifest: manifest.clone(),
            epoch: Arc::clone(&state_epoch),
            change_ring: VecDeque::new(),
            ring_bytes: 0,
            expired_cursors: 0,
            live_doc_bytes,
            index_bytes,
            pins: HashMap::new(),
            next_pin: 1,
            min_pinned_seq: None,
        };
        for event in replay_events {
            state.ring_bytes += event.approx_bytes;
            state.change_ring.push_back(event);
        }
        state.enforce_ring_caps(&config);

        let engine = RamEngine {
            dir: dir.to_path_buf(),
            config,
            io: Arc::clone(&io),
            state: RwLock::new(state),
            wal: Mutex::new(writer),
            commit_lock: Mutex::new(()),
            compaction_lock: Mutex::new(()),
            queue: Mutex::new(QueueState {
                queue: VecDeque::new(),
                leader_active: false,
            }),
            queue_cv: Condvar::new(),
            next_job: AtomicU64::new(1),
            poisoned: AtomicBool::new(false),
            poison_reason: Mutex::new(None),
            timings: Mutex::new(StageTimings::default()),
            auto_compact: Mutex::new(None),
            _lock: lock,
        };

        // Recovery rewrites a clean manifest: `pending_wal_gen` is gone (its
        // records now live in the active WAL), and any adopted schema state is
        // recorded so the next boot needs no WAL scanning to learn it.
        //
        // The clean manifest is also what the orphan sweep uses: `wal.gen_<p>`
        // is dead the moment the fold above re-committed its records into the
        // active generation, and keeping it referenced would leak a file that
        // holds nothing the manifest names.
        let adopted_pending = manifest.pending_wal_gen;
        let clean = if adopted_pending.is_some() || !schema_records.is_empty() {
            let mut clean = manifest.clone();
            clean.pending_wal_gen = None;
            {
                let state = engine.state.read();
                clean.last_seq = state.lineage.last_seq;
                clean.min_available_seq = state.lineage.min_available_seq;
            }
            write_ram_manifest_atomic(dir, &clean, io.as_ref())?;
            engine.state.write().manifest = clean.clone();
            clean
        } else {
            manifest
        };

        sweep_orphan_generations(dir, &clean, io.as_ref())?;
        Ok(engine)
    }

    // â”€â”€â”€ Reads â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    pub(crate) fn get(&self, id: &str) -> Result<Value> {
        let state = self.state.read();
        state
            .doc(id)
            .map(|record| record.doc.to_value())
            .ok_or_else(|| Error::not_found(id))
    }

    /// The exact JSON bytes of one document, straight from the RAM tree.
    ///
    /// Part of the engine's zero-parse read surface: no `Value` is built, the
    /// buffer is pre-sized from the record's own live-bytes estimate so a
    /// multi-megabyte conversation does not reallocate while serializing.
    pub(crate) fn get_raw_json(&self, id: &str) -> Result<Vec<u8>> {
        let state = self.state.read();
        let record = state.doc(id).ok_or_else(|| Error::not_found(id))?;
        let mut out = Vec::with_capacity(raw_json_capacity(record.approx_bytes));
        record.doc.write_json(&mut out);
        Ok(out)
    }

    /// Batch read, one entry per requested id (missing ids are `None`).
    #[allow(dead_code)]
    pub(crate) fn mget(&self, ids: &[String]) -> Vec<Option<Value>> {
        let state = self.state.read();
        ids.iter()
            .map(|id| state.doc(id).map(|record| record.doc.to_value()))
            .collect()
    }

    /// Batch read of the exact JSON bytes, one entry per requested id.
    pub(crate) fn mget_raw_json(&self, ids: &[String]) -> Vec<Option<Vec<u8>>> {
        let state = self.state.read();
        ids.iter()
            .map(|id| {
                state.doc(id).map(|record| {
                    let mut out = Vec::with_capacity(raw_json_capacity(record.approx_bytes));
                    record.doc.write_json(&mut out);
                    out
                })
            })
            .collect()
    }

    pub(crate) fn all_docs(&self) -> Vec<(String, Value)> {
        let state = self.state.read();
        let mut rows: Vec<(String, Value)> = state
            .docs
            .values()
            .map(|record| (record.id.to_string(), record.doc.to_value()))
            .collect();
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        rows
    }

    pub(crate) fn iter(&self) -> Vec<Value> {
        let state = self.state.read();
        let mut rows: Vec<&Arc<DocRecord>> = state.docs.values().collect();
        rows.sort_by(|a, b| a.id.cmp(&b.id));
        rows.into_iter()
            .map(|record| record.doc.to_value())
            .collect()
    }

    pub(crate) fn len(&self) -> usize {
        self.state.read().live_count()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.state.read().docs.is_empty()
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.state.read().docs.contains_key(id)
    }

    pub(crate) fn doc_seq(&self, id: &str) -> Option<u64> {
        self.state.read().doc(id).map(|record| record.doc_seq)
    }

    pub(crate) fn deleted_ids(&self) -> Vec<String> {
        let state = self.state.read();
        let mut ids: Vec<String> = state.deleted.iter().map(|id| id.to_string()).collect();
        ids.sort();
        ids
    }

    pub(crate) fn get_all_ids(&self) -> Vec<String> {
        let state = self.state.read();
        let mut ids: Vec<String> = state.docs.keys().map(|id| id.to_string()).collect();
        ids.sort();
        ids
    }

    pub(crate) fn is_deleted(&self, id: &str) -> bool {
        self.state.read().deleted.contains(id)
    }

    pub(crate) fn for_each_doc<F: FnMut(&str, &Value)>(&self, mut f: F) {
        let state = self.state.read();
        let mut records: Vec<&Arc<DocRecord>> = state.docs.values().collect();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        for record in records {
            let value = record.doc.to_value();
            f(record.id.as_ref(), &value);
        }
    }

    pub(crate) fn find(&self, field: &str, value: &Value) -> Vec<Value> {
        let state = self.state.read();
        let matches = |record: &DocRecord| match top_level(&record.doc, field) {
            Some(field_value) => docval::eq_json_value(field_value, value),
            None => false,
        };
        let mut out = Vec::new();
        if let Some(ids) = index_lookup(&state, field, value) {
            let mut ids: Vec<String> = ids.into_iter().collect();
            ids.sort();
            for id in ids {
                if let Some(record) = state.doc(&id) {
                    if matches(record) {
                        out.push(record.doc.to_value());
                    }
                }
            }
            return out;
        }
        let mut records: Vec<&Arc<DocRecord>> = state.docs.values().collect();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        for record in records {
            if matches(record) {
                out.push(record.doc.to_value());
            }
        }
        out
    }

    pub(crate) fn find_where<F>(&self, field: &str, predicate: F) -> Vec<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let state = self.state.read();
        let mut records: Vec<&Arc<DocRecord>> = state.docs.values().collect();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        let mut out = Vec::new();
        for record in records {
            if let Some(field_value) = top_level(&record.doc, field) {
                if predicate(&field_value.to_value()) {
                    out.push(record.doc.to_value());
                }
            }
        }
        out
    }

    pub(crate) fn find_range(&self, field: &str, min: &Value, max: &Value) -> Vec<Value> {
        let state = self.state.read();
        let lower = DocVal::from_value(min);
        let upper = DocVal::from_value(max);
        let in_range = |record: &DocRecord| {
            top_level(&record.doc, field)
                .map(|value| {
                    docval::cmp_docval(value, &lower) != std::cmp::Ordering::Less
                        && docval::cmp_docval(value, &upper) != std::cmp::Ordering::Greater
                })
                .unwrap_or(false)
        };
        let mut out = Vec::new();
        if let Some(ids) = index_range(
            &state,
            field,
            KeyBound::Included(min),
            KeyBound::Included(max),
        ) {
            let mut ids: Vec<String> = ids.into_iter().collect();
            ids.sort();
            for id in ids {
                if let Some(record) = state.doc(&id) {
                    if in_range(record) {
                        out.push(record.doc.to_value());
                    }
                }
            }
            return out;
        }
        let mut records: Vec<&Arc<DocRecord>> = state.docs.values().collect();
        records.sort_by(|a, b| a.id.cmp(&b.id));
        for record in records {
            if in_range(record) {
                out.push(record.doc.to_value());
            }
        }
        out
    }

    pub(crate) fn query(&self, ast: Value) -> Result<Vec<Value>> {
        self.query_with(ast, QueryOptions::default())
    }

    pub(crate) fn query_with(&self, ast: Value, opts: QueryOptions) -> Result<Vec<Value>> {
        let (_, page) = self.select_records(&ast, &opts)?;
        Ok(page
            .into_iter()
            .map(|record| record.doc.to_value())
            .collect())
    }

    pub(crate) fn query_projected(
        &self,
        ast: Value,
        opts: QueryOptions,
        fields: Option<&[String]>,
    ) -> Result<(usize, Vec<Value>)> {
        let (total, page) = self.select_records(&ast, &opts)?;
        let mut out = Vec::with_capacity(page.len());
        for record in page {
            out.push(match fields {
                Some(fields) => record.doc.project_to_value(fields),
                None => record.doc.to_value(),
            });
        }
        Ok((total, out))
    }

    pub(crate) fn query_raw(
        &self,
        ast: Value,
        opts: QueryOptions,
    ) -> Result<(usize, Vec<Arc<[u8]>>)> {
        self.query_raw_projected(ast, opts, None)
    }

    /// The selected page serialized straight out of the RAM trees â€” no
    /// `serde_json::Value` is built anywhere on this path.
    pub(crate) fn query_raw_projected(
        &self,
        ast: Value,
        opts: QueryOptions,
        fields: Option<&[String]>,
    ) -> Result<(usize, Vec<Arc<[u8]>>)> {
        let (total, page) = self.select_records(&ast, &opts)?;
        let mut out = Vec::with_capacity(page.len());
        for record in page {
            let mut buf = Vec::with_capacity(raw_json_capacity(record.approx_bytes));
            match fields {
                Some(fields) => record.doc.project_write_json(fields, &mut buf),
                None => record.doc.write_json(&mut buf),
            }
            out.push(Arc::from(buf));
        }
        Ok((total, out))
    }

    /// Select the matching page, holding the read lock only for selection:
    /// the returned `Arc<DocRecord>`s keep their trees alive after the lock is
    /// released, so serialization never runs under it.
    ///
    /// `offset` is honoured with or without a `limit` (herrbasan/nDB#15), and a
    /// set `limit` bounds the work: only `offset + limit` rows survive the
    /// partial selection before the page is sorted.
    fn select_records(
        &self,
        ast: &Value,
        opts: &QueryOptions,
    ) -> Result<(usize, Vec<Arc<DocRecord>>)> {
        validate_query_ast(ast)?;
        let state = self.state.read();
        let mut matches = collect_matches(&state, ast);
        let total = matches.len();

        if let Some((field, dir)) = &opts.sort_by {
            let cmp = |a: &Arc<DocRecord>, b: &Arc<DocRecord>| {
                let left = sort_key(a, field);
                let right = sort_key(b, field);
                let ordering = match dir {
                    SortDir::Asc => left.cmp(&right),
                    SortDir::Desc => right.cmp(&left),
                };
                ordering.then_with(|| a.id.cmp(&b.id))
            };
            let offset = opts.offset.unwrap_or(0);
            let take = match opts.limit {
                Some(limit) => offset.saturating_add(limit),
                None => matches.len(),
            };
            if take < matches.len() {
                matches.select_nth_unstable_by(take, &cmp);
                matches.truncate(take);
            }
            matches.sort_by(&cmp);
        }

        let offset = opts.offset.unwrap_or(0);
        let page: Vec<Arc<DocRecord>> = matches
            .into_iter()
            .skip(offset)
            .take(opts.limit.unwrap_or(usize::MAX))
            .collect();
        Ok((total, page))
    }

    pub(crate) fn facet_counts(&self, fields: &[String]) -> Vec<(String, BTreeMap<String, usize>)> {
        let mut counts: Vec<(String, BTreeMap<String, usize>)> =
            fields.iter().map(|f| (f.clone(), BTreeMap::new())).collect();
        let state = self.state.read();
        for record in state.docs.values() {
            for (field, map) in counts.iter_mut() {
                let Some(value) = top_level(&record.doc, field.as_str()) else {
                    continue;
                };
                match value {
                    DocVal::String(text) => {
                        *map.entry(text.to_string()).or_insert(0) += 1;
                    }
                    DocVal::Array(items) => {
                        for item in items.iter() {
                            if let DocVal::String(text) = item.as_ref() {
                                *map.entry(text.to_string()).or_insert(0) += 1;
                            }
                        }
                    }
                    _ => continue,
                }
            }
        }
        counts
    }

    pub(crate) fn lineage(&self) -> SourceLineage {
        self.state.read().lineage.clone()
    }

    pub(crate) fn snapshot_at(&self) -> Snapshot {
        let state = self.state.read();
        let mut docs: Vec<(String, Value)> = state
            .docs
            .values()
            .map(|record| (record.id.to_string(), record.doc.to_value()))
            .collect();
        docs.sort_by(|a, b| a.0.cmp(&b.0));
        Snapshot {
            lineage: state.lineage.clone(),
            docs,
        }
    }

    pub(crate) fn changes_since(&self, since_seq: u64) -> Result<ChangeBatch> {
        let mut state = self.state.write();
        let lineage = state.lineage.clone();
        if since_seq > lineage.last_seq {
            return Err(Error::CursorAhead {
                requested_seq: since_seq,
                last_seq: lineage.last_seq,
            });
        }
        if since_seq == lineage.last_seq {
            return Ok(ChangeBatch {
                lineage,
                events: Vec::new(),
            });
        }
        if since_seq.saturating_add(1) < lineage.min_available_seq {
            state.expired_cursors += 1;
            return Err(Error::CursorExpired {
                requested_seq: since_seq,
                min_available_seq: lineage.min_available_seq,
                last_seq: lineage.last_seq,
            });
        }
        let events: Vec<ChangeEvent> = state
            .change_ring
            .iter()
            .filter(|event| event.seq > since_seq)
            .map(|event| {
                debug_assert_eq!(
                    event.epoch.as_ref(),
                    lineage.source_epoch_uuid.as_str(),
                    "a retained change belongs to the lineage that issued it"
                );
                ChangeEvent {
                    seq: event.seq,
                    id: event.id.to_string(),
                    kind: match (&event.kind, &event.post_image) {
                        (RamChangeKind::Upsert, Some(record)) => {
                            ChangeKind::Upsert(record.doc.to_value())
                        }
                        _ => ChangeKind::Delete,
                    },
                }
            })
            .collect();
        Ok(ChangeBatch { lineage, events })
    }

    pub(crate) fn ram_stats(&self) -> RamStats {
        let wal_bytes = {
            let wal = self.wal.lock();
            wal.file_len().unwrap_or(0)
        };
        self.reap_expired_pins();
        let timings = self.timings.lock();
        let state = self.state.read();
        RamStats {
            live_doc_bytes: state.live_doc_bytes,
            index_bytes: state.index_bytes,
            ring_bytes: state.ring_bytes,
            oldest_retained_seq: state.lineage.min_available_seq,
            expired_cursors: state.expired_cursors,
            active_pins: state.pins.len(),
            poisoned: self.poisoned.load(Ordering::SeqCst),
            wal_bytes,
            batch_count: timings.batch_count,
            validate_us: timings.validate_us,
            queue_wait_us: timings.queue_wait_us,
            fsync_us: timings.fsync_us,
            publish_us: timings.publish_us,
        }
    }

    // â”€â”€â”€ Snapshot pins â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    /// Freeze the current document set and reserve its ring entries.
    ///
    /// Answering with `Vec<Arc<DocRecord>>` means pinning hundreds of MB costs
    /// a few hundred pointers, not a deep clone. The pin keeps the same list
    /// behind an `Arc`, so paging the export and the caller share one frozen
    /// document set â€” and neither of them touches live state.
    pub(crate) fn pin_snapshot(
        &self,
        ttl: Duration,
    ) -> Result<(u64, SourceLineage, Vec<Arc<DocRecord>>)> {
        if ttl.is_zero() {
            return Err(Error::invalid_arg(
                "a snapshot pin must live for a non-zero duration",
            ));
        }
        let expires_at = Instant::now().checked_add(ttl).ok_or_else(|| {
            Error::invalid_arg(format!("snapshot pin TTL {ttl:?} overflows the clock"))
        })?;
        let mut state = self.state.write();
        state.reap_expired_pins_locked();
        let lineage = state.lineage.clone();
        let mut docs: Vec<Arc<DocRecord>> = state.docs.values().cloned().collect();
        docs.sort_by(|a, b| a.id.cmp(&b.id));
        let docs = Arc::new(docs);
        let pin_id = state.next_pin;
        state.next_pin += 1;
        state.pins.insert(
            pin_id,
            Pin {
                cut_seq: lineage.last_seq,
                expires_at,
                lineage: lineage.clone(),
                docs: Arc::clone(&docs),
            },
        );
        state.recompute_min_pinned();
        Ok((pin_id, lineage, (*docs).clone()))
    }

    /// One page of a pinned snapshot.
    ///
    /// `None` covers both "never issued" and "expired": the caller answers
    /// `410 snapshot_expired` either way, because neither can be resumed.
    pub(crate) fn get_pinned_page(
        &self,
        pin_id: u64,
        offset: usize,
        limit: Option<usize>,
    ) -> Option<(SourceLineage, usize, Vec<Arc<DocRecord>>)> {
        let (lineage, docs) = {
            let mut state = self.state.write();
            state.reap_expired_pins_locked();
            let pin = state.pins.get(&pin_id)?;
            (pin.lineage.clone(), Arc::clone(&pin.docs))
        };
        let total = docs.len();
        let page = docs
            .iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .cloned()
            .collect();
        Some((lineage, total, page))
    }

    /// Release a pin. Unknown ids are reported, not ignored.
    pub(crate) fn release_pin(&self, pin_id: u64) -> Result<()> {
        let mut state = self.state.write();
        if state.pins.remove(&pin_id).is_none() {
            return Err(Error::invalid_arg(format!("unknown snapshot pin {pin_id}")));
        }
        state.recompute_min_pinned();
        Ok(())
    }

    /// Drop expired pins. Returns how many were reaped.
    ///
    /// The read-lock probe in front of the write lock matters: the server's
    /// reaper calls this every two seconds for every mounted database, and an
    /// idle database must not take a write lock just to learn it has nothing
    /// to do.
    pub(crate) fn reap_expired_pins(&self) -> usize {
        {
            let state = self.state.read();
            let now = Instant::now();
            if !state.pins.values().any(|pin| pin.expires_at <= now) {
                return 0;
            }
        }
        self.state.write().reap_expired_pins_locked()
    }

    // â”€â”€â”€ Public mutations â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    pub(crate) fn insert(&self, doc: Value, prefix: Option<String>) -> Result<String> {
        match self.submit(Job::Insert { doc, prefix })? {
            Reply::Id(id) => Ok(id),
            other => Err(unexpected_reply("insert", &other)),
        }
    }

    pub(crate) fn update(&self, id: &str, doc: Value, if_seq: Option<u64>) -> Result<()> {
        match self.submit(Job::Update {
            id: id.to_string(),
            doc,
            if_seq,
        })? {
            Reply::Unit => Ok(()),
            other => Err(unexpected_reply("update", &other)),
        }
    }

    pub(crate) fn set(&self, id: &str, path: &str, value: Value) -> Result<bool> {
        match self.submit(Job::Set {
            id: id.to_string(),
            path: path.to_string(),
            value,
        })? {
            Reply::Applied(applied) => Ok(applied),
            other => Err(unexpected_reply("set", &other)),
        }
    }

    pub(crate) fn remove(&self, id: &str, path: &str) -> Result<bool> {
        match self.submit(Job::Remove {
            id: id.to_string(),
            path: path.to_string(),
        })? {
            Reply::Applied(applied) => Ok(applied),
            other => Err(unexpected_reply("remove", &other)),
        }
    }

    pub(crate) fn array_push(&self, id: &str, field: &str, value: Value) -> Result<bool> {
        match self.submit(Job::Push {
            id: id.to_string(),
            field: field.to_string(),
            value,
        })? {
            Reply::Applied(applied) => Ok(applied),
            other => Err(unexpected_reply("array_push", &other)),
        }
    }

    pub(crate) fn apply_ops(
        &self,
        id: &str,
        ops: Vec<PatchOp>,
        if_seq: Option<u64>,
    ) -> Result<ApplyOpsResult> {
        match self.submit(Job::ApplyOps {
            id: id.to_string(),
            ops,
            if_seq,
        })? {
            Reply::Ops(result) => Ok(result),
            other => Err(unexpected_reply("apply_ops", &other)),
        }
    }

    pub(crate) fn delete(&self, id: &str) -> Result<()> {
        match self.submit(Job::Delete { id: id.to_string() })? {
            Reply::Unit => Ok(()),
            other => Err(unexpected_reply("delete", &other)),
        }
    }

    pub(crate) fn restore(&self, id: &str, doc: Value) -> Result<()> {
        match self.submit(Job::Restore {
            id: id.to_string(),
            doc,
        })? {
            Reply::Unit => Ok(()),
            other => Err(unexpected_reply("restore", &other)),
        }
    }

    // â”€â”€â”€ Indexes â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    pub(crate) fn create_index(&self, field: &str, kind: V2IndexKind) -> Result<()> {
        self.refuse_if_poisoned()?;
        let _commit = self.commit_lock.lock();
        {
            let state = self.state.read();
            if state.manifest.indexes.iter().any(|spec| spec.field == field) {
                return Err(Error::index_error(field, "index already exists"));
            }
        }
        // Build before the durable write so an unbuildable index fails without
        // touching the WAL.
        let built = {
            let state = self.state.read();
            RamIndex::build(state.docs.values().cloned(), field, kind)
        };
        let built_bytes = built.approx_bytes();

        self.append_schema_batch(&[RamWalRecord::index_add(field, kind)])?;

        let mut state = self.state.write();
        state.manifest.indexes.push(V2IndexSpec {
            field: field.to_string(),
            kind,
        });
        state.indexes.insert(field.to_string(), built);
        state.index_bytes += built_bytes;
        Ok(())
    }

    pub(crate) fn drop_index(&self, field: &str) -> Result<()> {
        self.refuse_if_poisoned()?;
        let _commit = self.commit_lock.lock();
        {
            let state = self.state.read();
            if !state.manifest.indexes.iter().any(|spec| spec.field == field) {
                return Err(Error::index_error(field, "index not found"));
            }
        }
        self.append_schema_batch(&[RamWalRecord::index_drop(field)])?;
        let mut state = self.state.write();
        state.manifest.indexes.retain(|spec| spec.field != field);
        if let Some(removed) = state.indexes.remove(field) {
            state.index_bytes = state.index_bytes.saturating_sub(removed.approx_bytes());
        }
        Ok(())
    }

    pub(crate) fn has_index(&self, field: &str) -> bool {
        self.has_hash_index(field) || self.has_btree_index(field)
    }

    pub(crate) fn has_hash_index(&self, field: &str) -> bool {
        self.state
            .read()
            .indexes
            .get(field)
            .map(|index| index.kind() == V2IndexKind::Hash)
            .unwrap_or(false)
    }

    pub(crate) fn has_btree_index(&self, field: &str) -> bool {
        self.state
            .read()
            .indexes
            .get(field)
            .map(|index| index.kind() == V2IndexKind::BTree)
            .unwrap_or(false)
    }

    // â”€â”€â”€ Durability & compaction â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    pub(crate) fn flush(&self) -> Result<()> {
        self.wal.lock().sync()
    }

    /// Fold the WAL into a new base generation â€” non-blocking for writers and
    /// readers.
    ///
    /// The three phases exist so that the only thing a writer ever waits for is
    /// two short critical sections, and the expensive part (serialising every
    /// document) runs with no lock held at all:
    ///
    /// 1. **Freeze & durable rotate** (commit lock held): capture the visible
    ///    sequence and the `Arc` handles of its documents, create
    ///    `wal.gen_<next>.ndb`, persist `pending_wal_gen = next` in the
    ///    manifest *before* the writer swaps, then swap. Writers now commit
    ///    into the new generation and readers keep reading RAM.
    /// 2. **Base serialization** (no lock): stream the frozen `DocRecord`s
    ///    into `base.gen_<next>.ndb`. A crash or error here leaves a manifest
    ///    whose `pending_wal_gen` still names a WAL that holds every write
    ///    acknowledged after the freeze â€” recovery replays base + WAL +
    ///    pending and loses nothing.
    /// 3. **Atomic publish** (short state lock): swap the manifest, advance
    ///    `min_available_seq` to the freeze point (but never past a live pin's
    ///    floor), prune the ring below it, then sweep the superseded files.
    ///
    /// A previous compaction that failed after its freeze leaves
    /// `pending_wal_gen` set; the retry reuses that generation as its target
    /// instead of rotating again, so no acknowledged write can be orphaned by a
    /// second rotation.
    pub(crate) fn compact(&self) -> Result<()> {
        self.refuse_if_poisoned()?;
        let _compaction = self.compaction_lock.lock();
        self.refuse_if_poisoned()?;

        // â”€â”€ Step 1: freeze the visible state, rotate durably â”€â”€â”€â”€â”€â”€â”€
        let (cut_seq, frozen_docs, next_gen) = {
            let _commit = self.commit_lock.lock();
            let (cut_seq, mut docs, mut manifest) = {
                let state = self.state.read();
                (
                    state.lineage.last_seq,
                    state.docs.values().cloned().collect::<Vec<_>>(),
                    state.manifest.clone(),
                )
            };
            docs.sort_by(|a, b| a.id.cmp(&b.id));

            match manifest.pending_wal_gen {
                Some(pending_gen) => {
                    // A stalled compaction already rotated: its generation is
                    // the active writer and holds every write made since. Reuse
                    // it as this compaction's target.
                    let active = self.wal.lock().wal_gen();
                    if active != pending_gen {
                        return Err(Error::corruption(
                            &self.dir,
                            format!(
                                "manifest names pending WAL generation {pending_gen} but the active writer is {active}"
                            ),
                        ));
                    }
                    (cut_seq, docs, pending_gen)
                }
                None => {
                    let next_gen = manifest.base_gen.max(manifest.wal_gen) + 1;
                    let new_wal = RamWalWriter::create_or_open(
                        &wal_file_path(&self.dir, next_gen),
                        next_gen,
                        Arc::clone(&self.io),
                    )?;
                    // The manifest must name the generation before any writer
                    // can reach it: a crash between the two steps then leaves
                    // an unreferenced stub (swept as an orphan), never writes
                    // nothing points at.
                    manifest.pending_wal_gen = Some(next_gen);
                    write_ram_manifest_atomic(&self.dir, &manifest, self.io.as_ref())?;
                    *self.wal.lock() = new_wal;
                    self.state.write().manifest = manifest;
                    (cut_seq, docs, next_gen)
                }
            }
        };

        // â”€â”€ Step 2: serialize the base with no lock held â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        write_ram_base_file(
            &base_file_path(&self.dir, next_gen),
            cut_seq,
            &frozen_docs,
            self.io.as_ref(),
        )?;
        drop(frozen_docs);

        // â”€â”€ Step 3: publish the swap â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        //
        // The manifest file is written with the generation fields this
        // compaction owns; the in-memory manifest is updated surgically under
        // the state lock rather than replaced, so an index DDL that landed
        // between the freeze and here is never dropped from RAM (its WAL
        // record makes the same change durable on the next boot).
        let (new_manifest, last_seq_now) = {
            let state = self.state.read();
            let mut manifest = state.manifest.clone();
            manifest.base_gen = next_gen;
            manifest.wal_gen = next_gen;
            manifest.pending_wal_gen = None;
            manifest.last_seq = state.lineage.last_seq;
            manifest.min_available_seq = cut_seq.saturating_add(1);
            (manifest, state.lineage.last_seq)
        };
        write_ram_manifest_atomic(&self.dir, &new_manifest, self.io.as_ref())?;
        {
            let mut state = self.state.write();
            state.reap_expired_pins_locked();
            // Compaction may advance the feed floor, but a pin's reservation
            // outranks it: entries at or after a live pin's `cut_seq` stay
            // readable for as long as that pin exists.
            let keep_floor = cut_seq
                .saturating_add(1)
                .min(state.min_pinned_seq.unwrap_or(u64::MAX));
            while let Some(front) = state.change_ring.front() {
                if front.seq >= keep_floor {
                    break;
                }
                if let Some(event) = state.change_ring.pop_front() {
                    state.ring_bytes = state.ring_bytes.saturating_sub(event.approx_bytes);
                }
            }
            state.lineage.min_available_seq = keep_floor;
            state.manifest.base_gen = next_gen;
            state.manifest.wal_gen = next_gen;
            state.manifest.pending_wal_gen = None;
            state.manifest.last_seq = last_seq_now.max(state.lineage.last_seq);
            state.manifest.min_available_seq = cut_seq.saturating_add(1);
            state.refresh_available_seq();
        }

        sweep_orphan_generations(&self.dir, &new_manifest, self.io.as_ref())?;
        Ok(())
    }

    /// Start WAL-growth-driven background compaction, when configured.
    ///
    /// Called by the `Database` that owns the engine, after the engine is
    /// behind an `Arc`. The thread holds only a `Weak`, so it can never keep a
    /// closed database alive, and `Drop` joins it.
    pub(crate) fn start_background(self: &Arc<Self>) {
        if self.config.auto_compact_wal_bytes.is_none() || self.auto_compact.lock().is_some() {
            return;
        }
        let threshold = self
            .config
            .auto_compact_wal_bytes
            .expect("checked just above");
        let stop = Arc::new(AutoCompactStop {
            flag: Mutex::new(false),
            wake: Condvar::new(),
        });
        let thread_stop = Arc::clone(&stop);
        let weak = Arc::downgrade(self);
        let handle = std::thread::Builder::new()
            .name("ndb-ram-compactor".to_string())
            .spawn(move || {
                let mut last_attempt = 0u64;
                loop {
                    {
                        let mut flag = thread_stop.flag.lock();
                        if *flag {
                            return;
                        }
                        thread_stop.wake.wait_for(&mut flag, AUTO_COMPACT_POLL);
                        if *flag {
                            return;
                        }
                    }
                    let Some(engine) = weak.upgrade() else {
                        return;
                    };
                    let len = match engine.wal.lock().file_len() {
                        Ok(len) => len,
                        Err(error) => {
                            report::detached("ram auto-compaction: stat WAL", error);
                            continue;
                        }
                    };
                    // Only act when the log grew since the last attempt: a
                    // failing disk must not turn into a hot retry loop.
                    if len < threshold || len <= last_attempt {
                        continue;
                    }
                    last_attempt = len;
                    match engine.compact() {
                        Ok(()) => last_attempt = 0,
                        Err(error) => report::detached("ram auto-compaction", error),
                    }
                }
            })
            .expect("spawn the RAM auto-compaction thread");
        *self.auto_compact.lock() = Some(AutoCompact {
            stop,
            handle: Mutex::new(Some(handle)),
        });
    }

    // â”€â”€â”€ Group commit â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

    fn refuse_if_poisoned(&self) -> Result<()> {
        if self.poisoned.load(Ordering::SeqCst) {
            let reason = self
                .poison_reason
                .lock()
                .clone()
                .unwrap_or_else(|| "unknown durable-write failure".to_string());
            return Err(Error::poisoned(reason));
        }
        Ok(())
    }

    fn poison(&self, error: &Error) {
        let mut reason = self.poison_reason.lock();
        if reason.is_none() {
            *reason = Some(error.to_string());
        }
        self.poisoned.store(true, Ordering::SeqCst);
    }

    fn submit(&self, job: Job) -> Result<Reply> {
        self.refuse_if_poisoned()?;
        let id = self.next_job.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut queue = self.queue.lock();
            queue.queue.push_back(QueuedJob {
                id,
                enqueued: Instant::now(),
                job,
                reply: tx,
            });
        }
        self.queue_cv.notify_all();
        self.drive(id, &rx)
    }

    fn drive(&self, my_id: u64, rx: &Receiver<Result<Reply>>) -> Result<Reply> {
        loop {
            {
                let mut queue = self.queue.lock();
                if !queue.leader_active {
                    let mine = queue.queue.iter().any(|queued| queued.id == my_id);
                    if !mine {
                        drop(queue);
                        return rx.recv().unwrap_or_else(|_| {
                            Err(Error::Serialization(
                                "group commit leader dropped the reply channel".into(),
                            ))
                        });
                    }
                    queue.leader_active = true;
                } else {
                    while queue.leader_active {
                        self.queue_cv.wait(&mut queue);
                    }
                    drop(queue);
                    if let Ok(reply) = rx.try_recv() {
                        return reply;
                    }
                    continue;
                }
            }

            let batch: Vec<QueuedJob> = {
                let mut queue = self.queue.lock();
                queue.queue.drain(..).collect()
            };
            let results = self.commit_batch(&batch);
            for (queued, result) in batch.into_iter().zip(results) {
                let _ = queued.reply.send(result);
            }
            {
                let mut queue = self.queue.lock();
                queue.leader_active = false;
            }
            self.queue_cv.notify_all();

            if let Ok(reply) = rx.try_recv() {
                return reply;
            }
            // Our job arrived after the drain: loop and lead again.
        }
    }

    fn commit_batch(&self, batch: &[QueuedJob]) -> Vec<Result<Reply>> {
        if self.poisoned.load(Ordering::SeqCst) {
            let reason = self
                .poison_reason
                .lock()
                .clone()
                .unwrap_or_else(|| "unknown durable-write failure".to_string());
            return batch
                .iter()
                .map(|_| Err(Error::poisoned(reason.clone())))
                .collect();
        }
        let _commit = self.commit_lock.lock();

        let picked_up = Instant::now();
        {
            let mut timings = self.timings.lock();
            for queued in batch {
                timings.queue_wait_us += picked_up
                    .saturating_duration_since(queued.enqueued)
                    .as_micros() as u64;
            }
        }

        let validate_start = Instant::now();
        let start_seq = self.state.read().lineage.last_seq;
        let mut stage = Stage::new(start_seq);
        let mut results: Vec<Result<Reply>> = Vec::with_capacity(batch.len());
        {
            let state = self.state.read();
            for queued in batch {
                let saved_overlay = stage.overlay.clone();
                let saved_records = stage.records.len();
                let saved_pubs = stage.pubs.len();
                let saved_seq = stage.next_seq;
                let saved_delta = stage.live_delta;
                let saved_ring = stage.ring_add_bytes;
                match stage.run(&state, &queued.job) {
                    Ok(reply) => results.push(Ok(reply)),
                    Err(error) => {
                        // A failed job stages nothing; the rest of the batch
                        // continues in FIFO order.
                        stage.overlay = saved_overlay;
                        stage.records.truncate(saved_records);
                        stage.pubs.truncate(saved_pubs);
                        stage.next_seq = saved_seq;
                        stage.live_delta = saved_delta;
                        stage.ring_add_bytes = saved_ring;
                        results.push(Err(error));
                    }
                }
            }
        }
        self.timings.lock().validate_us += validate_start.elapsed().as_micros() as u64;

        if stage.records.is_empty() {
            // Every job was a no-op or failed: nothing is durable, nothing is
            // visible, no `seq` is consumed.
            return results;
        }

        // â”€â”€ Memory governance, before anything reaches the WAL â”€â”€â”€â”€â”€â”€
        if let Err(rejected) = self.admit(&stage) {
            return results
                .into_iter()
                .map(|result| match result {
                    Ok(_) => Err(duplicate_error(&rejected)),
                    Err(error) => Err(error),
                })
                .collect();
        }

        // â”€â”€ The durable step â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
        let fsync_start = Instant::now();
        let prepared = {
            let mut wal = self.wal.lock();
            let prepared = wal.prepare_batch(&stage.records);
            let start_len = wal.file_len();
            (prepared, start_len)
        };
        let (prepared, start_len) = prepared;
        let (buffer, end_seq) = match prepared {
            Ok(prepared) => prepared,
            Err(error) => {
                return results
                    .into_iter()
                    .map(|result| match result {
                        Ok(_) => Err(duplicate_error(&error)),
                        Err(error) => Err(error),
                    })
                    .collect();
            }
        };
        let start_len = match start_len {
            Ok(len) => len,
            Err(error) => {
                self.poison(&error);
                return results
                    .into_iter()
                    .map(|result| match result {
                        Ok(_) => Err(duplicate_error(&error)),
                        Err(error) => Err(error),
                    })
                    .collect();
            }
        };

        let durable = self.durable_append(&buffer, end_seq, start_len);
        self.timings.lock().fsync_us += fsync_start.elapsed().as_micros() as u64;

        match durable {
            Ok(()) => {
                // â”€â”€ Publish, once, after the fsync â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€
                let publish_start = Instant::now();
                self.publish(stage, end_seq);
                let mut timings = self.timings.lock();
                timings.publish_us += publish_start.elapsed().as_micros() as u64;
                timings.batch_count += 1;
                results
            }
            Err(error) => {
                // Nothing was published. Every caller whose job staged
                // successfully hears the same failure, and the database is now
                // poisoned.
                self.poison(&error);
                results
                    .into_iter()
                    .map(|result| match result {
                        Ok(_) => Err(duplicate_error(&error)),
                        Err(error) => Err(error),
                    })
                    .collect()
            }
        }
    }

    /// Write one prepared batch and fsync it, retrying only a transient
    /// failure â€” and rolling the partial bytes back before each retry.
    fn durable_append(&self, buffer: &[u8], end_seq: u64, start_len: u64) -> Result<()> {
        let mut wal = self.wal.lock();
        let path = wal.path().to_path_buf();
        let mut attempt = 0;
        loop {
            attempt += 1;
            match wal.append_prepared_and_sync(buffer, end_seq) {
                Ok(()) => return Ok(()),
                Err(error) => {
                    if attempt >= DURABLE_RETRIES || !transient_io(&error) {
                        return Err(error);
                    }
                    // The batch never committed: remove its bytes so the retry
                    // cannot extend a partial write, then write the full buffer
                    // again from the same offset.
                    if let Err(rollback) = self.io.truncate_and_sync(&path, start_len) {
                        return Err(Error::poisoned(format!(
                            "cannot roll back a failed WAL append at {}: {rollback}",
                            path.display()
                        )));
                    }
                    wal.rewind_to(start_len)?;
                    std::thread::sleep(Duration::from_millis(1 + attempt as u64 * 2));
                }
            }
        }
    }

    /// Enforce the RAM budget against the post-commit estimate.
    ///
    /// Unpinned change-ring history is evicted first; if the write still would
    /// not fit, it is refused before the WAL is touched.
    fn admit(&self, stage: &Stage) -> Result<()> {
        let limit = match self.config.max_memory_bytes {
            Some(limit) => limit,
            None => return Ok(()),
        };
        let mut state = self.state.write();
        state.reap_expired_pins_locked();
        let projected_live = state.live_doc_bytes as i128 + stage.live_delta as i128;
        let projected_live = usize::try_from(projected_live.max(0)).unwrap_or(usize::MAX);
        let projected = projected_live
            .saturating_add(state.index_bytes)
            .saturating_add(state.ring_bytes.saturating_add(stage.ring_add_bytes));
        if projected <= limit {
            return Ok(());
        }
        // Under pressure, retain only what is pinned.
        loop {
            let pinned = match state.change_ring.front() {
                None => break,
                Some(event) => state
                    .min_pinned_seq
                    .map_or(false, |floor| event.seq >= floor),
            };
            if pinned {
                break;
            }
            if let Some(event) = state.change_ring.pop_front() {
                state.ring_bytes = state.ring_bytes.saturating_sub(event.approx_bytes);
            }
        }
        state.refresh_available_seq();
        let projected = projected_live
            .saturating_add(state.index_bytes)
            .saturating_add(state.ring_bytes.saturating_add(stage.ring_add_bytes));
        if projected <= limit {
            return Ok(());
        }
        Err(Error::MemoryLimitExceeded {
            current_bytes: state.memory_used(),
            limit_bytes: limit,
            needed_bytes: projected.saturating_sub(limit),
        })
    }

    /// Apply a staged batch to RAM. Called only after the fsync succeeded, in
    /// exactly one critical section, so documents, indexes, the watermark and
    /// the change ring move together.
    fn publish(&self, stage: Stage, last_seq: u64) {
        let mut state = self.state.write();
        state.reap_expired_pins_locked();
        for op in stage.pubs {
            match op {
                PubOp::Put { record, ring_bytes } => {
                    if let Some(old) = state.docs.get(&record.id).cloned() {
                        state.unindex_doc(&old);
                        state.live_doc_bytes =
                            state.live_doc_bytes.saturating_sub(old.approx_bytes);
                    }
                    state.index_doc(&record);
                    state.live_doc_bytes += record.approx_bytes;
                    state.deleted.remove(&record.id);
                    let event = RamChangeEvent {
                        seq: record.doc_seq,
                        epoch: Arc::clone(&state.epoch),
                        kind: RamChangeKind::Upsert,
                        id: Arc::clone(&record.id),
                        approx_bytes: ring_bytes,
                        post_image: Some(Arc::clone(&record)),
                    };
                    state.docs.insert(Arc::clone(&record.id), record);
                    state.push_event(event, &self.config);
                }
                PubOp::Delete {
                    id,
                    seq,
                    ring_bytes,
                } => {
                    if let Some(old) = state.docs.remove(&id) {
                        state.unindex_doc(&old);
                        state.live_doc_bytes =
                            state.live_doc_bytes.saturating_sub(old.approx_bytes);
                    }
                    state.deleted.insert(Arc::clone(&id));
                    let event = RamChangeEvent {
                        seq,
                        epoch: Arc::clone(&state.epoch),
                        kind: RamChangeKind::Delete,
                        approx_bytes: ring_bytes,
                        id,
                        post_image: None,
                    };
                    state.push_event(event, &self.config);
                }
            }
        }
        state.lineage.last_seq = state.lineage.last_seq.max(last_seq);
        state.refresh_available_seq();
    }

    fn append_schema_batch(&self, records: &[RamWalRecord]) -> Result<()> {
        let (buffer, end_seq, start_len) = {
            let mut wal = self.wal.lock();
            let (buffer, end_seq) = wal.prepare_batch(records)?;
            let start_len = wal.file_len()?;
            (buffer, end_seq, start_len)
        };
        self.durable_append(&buffer, end_seq, start_len)
    }
}

impl Drop for RamEngine {
    /// Stop and join the background compactor.
    ///
    /// A thread mid-compaction holds a strong `Arc` (it upgraded a `Weak`), so
    /// `Drop` cannot run *inside* `compact()`. It can, however, run on the
    /// compactor thread itself â€” when that thread's `Arc` is the last one â€” and
    /// joining a thread from inside itself would deadlock. The handle is only
    /// joined from a different thread; a self-join is skipped, and the loop
    /// sees the stop flag on its next iteration.
    fn drop(&mut self) {
        let auto = self.auto_compact.lock().take();
        if let Some(auto) = auto {
            *auto.stop.flag.lock() = true;
            auto.stop.wake.notify_all();
            if let Some(handle) = auto.handle.lock().take() {
                if handle.thread().id() == std::thread::current().id() {
                    return;
                }
                if handle.join().is_err() {
                    report::detached("ram auto-compaction thread", "panicked while joining");
                }
            }
        }
    }
}

fn unexpected_reply(op: &str, reply: &Reply) -> Error {
    Error::Serialization(format!(
        "ram group commit returned an unexpected {} reply to {op}",
        reply.label()
    ))
}

fn duplicate_error(error: &Error) -> Error {
    match error {
        Error::Io {
            source,
            path,
            context,
        } => Error::Io {
            source: std::io::Error::new(source.kind(), source.to_string()),
            path: path.clone(),
            context: context.clone(),
        },
        Error::Corruption { file, message } => Error::Corruption {
            file: file.clone(),
            message: message.clone(),
        },
        Error::NotFound { id } => Error::NotFound { id: id.clone() },
        Error::InvalidArgument { reason } => Error::InvalidArgument {
            reason: reason.clone(),
        },
        Error::Serialization(message) => Error::Serialization(message.clone()),
        Error::DatabaseLocked { path } => Error::DatabaseLocked { path: path.clone() },
        Error::IndexError { field, reason } => Error::IndexError {
            field: field.clone(),
            reason: reason.clone(),
        },
        Error::BucketError { reason } => Error::BucketError {
            reason: reason.clone(),
        },
        Error::PolicyViolation { reason } => Error::PolicyViolation {
            reason: reason.clone(),
        },
        Error::CursorExpired {
            requested_seq,
            min_available_seq,
            last_seq,
        } => Error::CursorExpired {
            requested_seq: *requested_seq,
            min_available_seq: *min_available_seq,
            last_seq: *last_seq,
        },
        Error::CursorAhead {
            requested_seq,
            last_seq,
        } => Error::CursorAhead {
            requested_seq: *requested_seq,
            last_seq: *last_seq,
        },
        Error::Conflict { current_seq, doc } => Error::Conflict {
            current_seq: *current_seq,
            doc: doc.clone(),
        },
        Error::Poisoned { reason } => Error::Poisoned {
            reason: reason.clone(),
        },
        Error::MemoryLimitExceeded {
            current_bytes,
            limit_bytes,
            needed_bytes,
        } => Error::MemoryLimitExceeded {
            current_bytes: *current_bytes,
            limit_bytes: *limit_bytes,
            needed_bytes: *needed_bytes,
        },
    }
}

// â”€â”€â”€ Query helpers â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

/// A top-level field read, matching `serde_json::Value::get` (no dot-path
/// interpretation) â€” the semantics `find`, `find_where` and facets have always
/// had.
fn top_level<'a>(doc: &'a DocVal, field: &str) -> Option<&'a DocVal> {
    match doc {
        DocVal::Object(map) => map.get(field).map(|value| value.as_ref()),
        _ => None,
    }
}

/// Sort key for one document, resolving a dot path and treating a missing
/// field as JSON null â€” exactly what the reference model does.
fn sort_key(record: &DocRecord, field: &str) -> OrderedValue {
    match record.doc.get_path(field) {
        Some(value) => value.ordered_key(),
        None => OrderedValue::Null,
    }
}

impl IndexAccess for RamState {
    fn lookup(&self, field: &str, value: &Value) -> Option<HashSet<String>> {
        index_lookup(self, field, value)
    }

    fn range_scan(
        &self,
        field: &str,
        min: KeyBound<'_>,
        max: KeyBound<'_>,
    ) -> Option<HashSet<String>> {
        index_range(self, field, min, max)
    }

    fn in_values(&self, field: &str, operand: &Value) -> Option<HashSet<String>> {
        let values = operand.as_array()?;
        if values.is_empty() {
            index_lookup(self, field, &Value::Null)?;
            return Some(HashSet::new());
        }
        let mut out = HashSet::new();
        let mut indexed = false;
        for value in values {
            if let Some(ids) = index_lookup(self, field, value) {
                indexed = true;
                out.extend(ids);
            }
        }
        indexed.then_some(out)
    }
}

fn index_lookup(state: &RamState, field: &str, value: &Value) -> Option<HashSet<String>> {
    let index = state.indexes.get(field)?;
    let keys = index_keys(value);
    Some(index.lookup(&keys).into_iter().collect())
}

fn index_range(
    state: &RamState,
    field: &str,
    min: KeyBound<'_>,
    max: KeyBound<'_>,
) -> Option<HashSet<String>> {
    let index = state.indexes.get(field)?;
    let to_bound = |bound: KeyBound<'_>| match bound {
        KeyBound::Unbounded => Bound::Unbounded,
        KeyBound::Included(value) => Bound::Included(ordered_value(value)),
        KeyBound::Excluded(value) => Bound::Excluded(ordered_value(value)),
    };
    let ids = index.range(to_bound(min), to_bound(max))?;
    Some(ids.into_iter().collect())
}

/// Every matching record, id-ascending.
///
/// The index only narrows: each candidate is re-checked with `matches_filter`,
/// which is the same predicate the engine has always applied. The id ordering
/// makes an unsorted page deterministic and ties in a sorted page stable.
fn collect_matches(state: &RamState, ast: &Value) -> Vec<Arc<DocRecord>> {
    match plan_candidates(ast, state) {
        Some(ids) => {
            let mut ids: Vec<&String> = ids.iter().collect();
            ids.sort();
            ids.into_iter()
                .filter_map(|id| state.doc(id))
                .filter(|record| record.doc.matches_filter(ast))
                .cloned()
                .collect()
        }
        None => {
            let mut records: Vec<Arc<DocRecord>> = state.docs.values().cloned().collect();
            records.sort_by(|a, b| a.id.cmp(&b.id));
            records
                .into_iter()
                .filter(|record| record.doc.matches_filter(ast))
                .collect()
        }
    }
}

// â”€â”€â”€ Recovery helpers â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€â”€

fn record_id(op: &RamWalOp) -> String {
    match op {
        RamWalOp::Insert { id, .. }
        | RamWalOp::Replace { id, .. }
        | RamWalOp::Patch { id, .. }
        | RamWalOp::Delete { id }
        | RamWalOp::Restore { id, .. } => id.clone(),
        RamWalOp::IndexAdd { field, .. } | RamWalOp::IndexDrop { field } => field.clone(),
    }
}

/// The post-image a replayed upsert must leave live.
fn post_after_replay<'a>(
    post: Option<&'a Arc<DocRecord>>,
    seq: u64,
    id: &str,
) -> Result<&'a Arc<DocRecord>> {
    post.ok_or_else(|| {
        Error::corruption(
            PathBuf::from("<ram WAL>"),
            format!("upsert at seq {seq} for {id:?} left no live document"),
        )
    })
}

/// Apply one committed WAL record to the recovered document set.
fn apply_record(
    docs: &mut HashMap<Arc<str>, Arc<DocRecord>>,
    deleted: &mut HashSet<Arc<str>>,
    live_doc_bytes: &mut usize,
    seq: u64,
    record: &RamWalRecord,
) -> Result<()> {
    match &record.op {
        RamWalOp::Insert { id, doc_json }
        | RamWalOp::Replace { id, doc_json }
        | RamWalOp::Restore { id, doc_json } => {
            let value: Value = serde_json::from_slice(doc_json).map_err(|e| {
                Error::corruption(
                    PathBuf::from("<ram WAL>"),
                    format!("WAL frame at seq {seq} for {id:?} is not valid JSON: {e}"),
                )
            })?;
            let key: Arc<str> = Arc::from(id.as_str());
            if let Some(old) = docs.remove(&key) {
                *live_doc_bytes = live_doc_bytes.saturating_sub(old.approx_bytes);
            }
            let record = DocRecord::new(Arc::clone(&key), seq, DocVal::from_value(&value));
            *live_doc_bytes += record.approx_bytes;
            docs.insert(key.clone(), record);
            deleted.remove(&key);
        }
        RamWalOp::Patch { id, ops } => {
            let key: Arc<str> = Arc::from(id.as_str());
            let previous = docs.get(&key).cloned().ok_or_else(|| {
                Error::corruption(
                    PathBuf::from("<ram WAL>"),
                    format!("patch at seq {seq} names {id:?}, which is not live"),
                )
            })?;
            let mut tree = Arc::clone(&previous.doc);
            let mut total_delta = 0isize;
            for op in ops {
                let (next, applied, delta) = match op {
                    RamWalPatchOp::Set { path, value_json } => {
                        let value: Value = serde_json::from_slice(value_json).map_err(|e| {
                            Error::corruption(
                                PathBuf::from("<ram WAL>"),
                                format!("patch value at seq {seq} is not JSON: {e}"),
                            )
                        })?;
                        docval::set_path(&tree, path, DocVal::from_value(&value))?
                    }
                    RamWalPatchOp::Remove { path } => docval::remove_path(&tree, path)?,
                    RamWalPatchOp::ArrayPush { path, value_json } => {
                        let value: Value = serde_json::from_slice(value_json).map_err(|e| {
                            Error::corruption(
                                PathBuf::from("<ram WAL>"),
                                format!("patch value at seq {seq} is not JSON: {e}"),
                            )
                        })?;
                        docval::array_push_path(&tree, path, DocVal::from_value(&value))?
                    }
                };
                if !applied {
                    return Err(Error::corruption(
                        PathBuf::from("<ram WAL>"),
                        format!("patch at seq {seq} for {id:?} did not resolve"),
                    ));
                }
                tree = next;
                total_delta += delta;
            }
            let next = DocRecord::with_delta(&previous, seq, tree, total_delta);
            *live_doc_bytes = (*live_doc_bytes)
                .saturating_sub(previous.approx_bytes)
                .saturating_add(next.approx_bytes);
            docs.insert(key, next);
        }
        RamWalOp::Delete { id } => {
            let key: Arc<str> = Arc::from(id.as_str());
            if let Some(old) = docs.remove(&key) {
                *live_doc_bytes = live_doc_bytes.saturating_sub(old.approx_bytes);
            }
            deleted.insert(key);
        }
        RamWalOp::IndexAdd { .. } | RamWalOp::IndexDrop { .. } => {
            // Schema operations carry no document state; the manifest is
            // their authority and `open_with_io` adopts them there.
        }
    }
    Ok(())
}

/// Create `base.gen_1.ndb` (empty), `wal.gen_1.ndb` (header) and a v3
/// `manifest.json`.
///
/// A folder with no manifest is not a database. Any file an earlier failed
/// initialisation left under the exact names this function writes is removed
/// first, so a retry cannot trip over a half-written generation.
fn initialise(dir: &Path, io: &Arc<dyn Io>) -> Result<RamManifest> {
    // A v1 journal in this folder means the caller pointed a v3 open at a
    // database that already holds data. Creating fresh v3 files beside it would
    // silently shadow those documents.
    let leftover_journal = dir.join("data.jsonl");
    if leftover_journal.exists() {
        return Err(Error::corruption(
            &leftover_journal,
            "a v1 journal exists here, so this folder is not a new v3 database; open it as v1 or migrate it first",
        ));
    }

    let base = base_file_path(dir, 1);
    let wal = wal_file_path(dir, 1);
    let manifest_path = ram_manifest_path(dir);
    for path in [&base, &wal, &manifest_path] {
        if path.exists() {
            io.remove_file(path)
                .map_err(Error::io_err(path, "remove incomplete v3 initialisation"))?;
        }
    }

    write_ram_base_file(&base, 0, &[], io.as_ref())?;
    let writer = RamWalWriter::create_or_open(&wal, 1, Arc::clone(io))?;
    drop(writer);

    let manifest = RamManifest {
        version: RAM_STORAGE_VERSION,
        source_epoch_uuid: storage::generate_epoch_uuid(),
        base_gen: 1,
        wal_gen: 1,
        pending_wal_gen: None,
        last_seq: 0,
        min_available_seq: 1,
        indexes: Vec::new(),
    };
    write_ram_manifest_atomic(dir, &manifest, io.as_ref())?;
    Ok(manifest)
}

/// Delete `base.gen_*` / `wal.gen_*` files the active manifest does not name.
///
/// `pending_wal_gen` is spared too: a stalled compaction's target is the one
/// generation that may hold acknowledged writes without being the active one
/// yet.
fn sweep_orphan_generations(dir: &Path, manifest: &RamManifest, io: &dyn Io) -> Result<()> {
    let keep_base = manifest.base_gen;
    let keep_wal = [manifest.wal_gen, manifest.pending_wal_gen.unwrap_or(u64::MAX)];
    let entries = std::fs::read_dir(dir).map_err(Error::io_err(dir, "list v3 database folder"))?;
    for entry in entries {
        let entry = entry.map_err(Error::io_err(dir, "read v3 folder entry"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let orphan = if let Some(rest) = name.strip_prefix("base.gen_") {
            rest.strip_suffix(".ndb")
                .and_then(|gen| gen.parse::<u64>().ok())
                .map(|gen| gen != keep_base)
        } else if let Some(rest) = name.strip_prefix("wal.gen_") {
            rest.strip_suffix(".ndb")
                .and_then(|gen| gen.parse::<u64>().ok())
                .map(|gen| !keep_wal.contains(&gen))
        } else {
            None
        };
        if orphan == Some(true) {
            let path = entry.path();
            io.remove_file(&path)
                .map_err(Error::io_err(&path, "remove orphaned v3 generation"))?;
        }
    }
    io.sync_parent_dir(&dir.join(wal_file_name(manifest.wal_gen)))
        .map_err(Error::io_err(dir, "sync directory after orphan sweep"))?;
    Ok(())
}
