//! The v2 binary/mmap document engine.
//!
//! Slice 3A built the on-disk artefacts (`manifest.json`, `base.gen_<N>.ndb`,
//! `wal.gen_<N>.ndb`) and their CRC-checked parsers. This module wires them
//! into a live database:
//!
//! - **Resting documents stay off the heap.** A document is either a
//!   [`V2Slot::Base`] (a slice of the immutable `MmapBase`) or a
//!   [`V2Slot::Wal`] (compact UTF-8 bytes of a post-compaction write). Reading
//!   one document parses one document — never the whole database.
//! - **Group commit.** Concurrent writers queue a [`Job`]; the first free
//!   thread becomes leader, drains the queue, stages every mutation in FIFO
//!   order against the live state, writes the whole batch with one
//!   `append_batch_and_sync`, and only then publishes it to RAM. A failed
//!   fsync publishes nothing and every waiter hears the error.
//! - **Bounded change ring.** The in-RAM change feed is capped at
//!   [`CHANGE_RING_CAP`] events; an older cursor is answered by streaming the
//!   WAL from disk.
//! - **Non-blocking compaction.** Readers and writers keep running while the
//!   new base generation is written; a mirror WAL captures writes that land
//!   after the freeze point, and the swap is a short critical section.
//!
//! The typed `OrderedValue` index structures and the query planner live in
//! `lib.rs` and are shared with the v1 engine, not duplicated here.

use crate::error::{Error, Result};
use crate::report;
use crate::search::{self, TextIndex, TextSearch};
use crate::storage::{self, ChangeBatch, ChangeEvent, ChangeKind, Snapshot, SourceLineage};
use crate::v2_format;
use crate::v2_format::{
    base_file_path, read_manifest, replay_wal, scan_wal_since, wal_file_path, write_base_file,
    write_manifest_atomic, BaseDocInput, BaseIndexEntry, MmapBase, PersistedIndex, V2IndexKind,
    V2IndexSpec, V2Manifest, WalOp, WalRecord, WalWriter,
};
use crate::v2_json_scan;
use crate::{
    apply_path_push, apply_path_remove, apply_path_set, validate_query_ast, value_cmp, values_equal,
    ApplyOpsResult, BTreeIndex, BucketPolicy, HashIndex, Index, KeyBound, LoggedChange, LoggedKind,
    OnDocumentDelete, PatchOp, QueryOptions, SortDir, TrashMode,
};
use parking_lot::{Condvar, Mutex, RwLock};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender};
use std::sync::Arc;

/// Maximum number of committed events kept in RAM. Older cursors are answered
/// from the WAL on disk.
pub const CHANGE_RING_CAP: usize = 1024;

/// Maximum number of parsed documents kept for repeated reads.
///
/// A v2 document rests as bytes; materializing it means one
/// `serde_json::from_slice`. Steady-state query workloads read the same
/// documents over and over, so the most recently materialized ones are kept as
/// parsed values — bounded, and validated by the document's commit sequence so
/// a mutation can never be served from a stale entry. Cold boot allocates
/// none of it: the cache fills only from documents a caller actually read.
const PARSED_CACHE_CAP: usize = 8192;

// ─── Slots ──────────────────────────────────────────────────────────

/// A document written since the last compaction.
///
/// Resting base documents are *not* represented here: they live in the mapped
/// directory and are reached by binary search, so a cold open with an empty WAL
/// allocates nothing per document.
#[derive(Debug, Clone)]
struct WalEntry {
    /// Compact UTF-8 JSON bytes.
    bytes: Arc<[u8]>,
    /// Commit sequence of the document's last mutation.
    doc_seq: u64,
}

// ─── Engine state ───────────────────────────────────────────────────

/// The manifest and everything derived from it. Compaction replaces it as one
/// unit, so a reader never observes a new base with old slots.
struct V2State {
    manifest: V2Manifest,
    base: Arc<MmapBase>,
    /// Bit `slot` set = the base document at that slot was updated or deleted
    /// since this base was written. `ceil(slot_count / 64)` words — 12.5 KB for
    /// 100 000 documents, versus one heap object per document before.
    superseded: Vec<u64>,
    /// Number of set bits in `superseded`.
    suppressed: u32,
    /// Post-compaction writes and tombstones only — empty at cold boot.
    wal_overlay: HashMap<Box<str>, WalEntry>,
    deleted: HashSet<String>,
    lineage: SourceLineage,
}

impl V2State {
    fn superseded(&self, slot: u32) -> bool {
        let word = (slot / 64) as usize;
        let bit = 1u64 << (slot % 64);
        self.superseded.get(word).map_or(false, |value| value & bit != 0)
    }

    fn mark_superseded(&mut self, slot: u32) {
        let word = (slot / 64) as usize;
        let bit = 1u64 << (slot % 64);
        if self.superseded[word] & bit == 0 {
            self.superseded[word] |= bit;
            self.suppressed += 1;
        }
    }

    fn slot_count(&self) -> u32 {
        self.base.slot_count()
    }

    /// Live documents: base slots minus the superseded ones, plus every
    /// WAL-overlay entry.
    fn live_count(&self) -> usize {
        self.slot_count() as usize - self.suppressed as usize + self.wal_overlay.len()
    }

    /// The bytes and commit sequence of one live document.
    ///
    /// Overlay first (post-compaction writes shadow their base version), then
    /// one binary search in the mapped directory.
    fn doc(&self, id: &str) -> Option<(&[u8], u64)> {
        if let Some(entry) = self.wal_overlay.get(id) {
            return Some((entry.bytes.as_ref(), entry.doc_seq));
        }
        let slot = self.base.find_slot(id)?;
        if self.superseded(slot) {
            return None;
        }
        let entry = self.base.entry_by_slot(slot);
        Some((
            self.base.doc_bytes(entry.data_offset, entry.data_len),
            entry.doc_seq,
        ))
    }

    /// Every live document in a deterministic order: base slot order (ids
    /// ascending) followed by overlay ids ascending.
    fn for_each_live<F>(&self, mut f: F) -> Result<()>
    where
        F: FnMut(&str, &[u8], u64) -> Result<()>,
    {
        for slot in 0..self.slot_count() {
            if self.superseded(slot) {
                continue;
            }
            let entry = self.base.entry_by_slot(slot);
            f(
                entry.id,
                self.base.doc_bytes(entry.data_offset, entry.data_len),
                entry.doc_seq,
            )?;
        }
        if !self.wal_overlay.is_empty() {
            let mut ids: Vec<&str> = self.wal_overlay.keys().map(|id| id.as_ref()).collect();
            ids.sort_unstable();
            for id in ids {
                let entry = &self.wal_overlay[id];
                f(id, entry.bytes.as_ref(), entry.doc_seq)?;
            }
        }
        Ok(())
    }
}

/// The primary WAL plus, during compaction, the mirror generation that must
/// receive every write landing after the freeze point.
struct WalState {
    primary: WalWriter,
    mirror: Option<WalWriter>,
}

// ─── Group commit ───────────────────────────────────────────────────

/// What a committed job returns to its caller.
pub(crate) enum Reply {
    Id(String),
    Applied(bool),
    Ops(ApplyOpsResult),
    Unit,
}

/// One queued mutation. The job carries the *intent*; the read-modify-write
/// runs on the leader thread so concurrent writers cannot interleave.
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
    job: Job,
    reply: Sender<Result<Reply>>,
}

struct QueueState {
    queue: VecDeque<QueuedJob>,
    leader_active: bool,
}

/// One staged mutation, applied to RAM only after the batch fsync succeeded.
enum AppliedOp {
    Put {
        id: String,
        bytes: Arc<[u8]>,
        value: Arc<Value>,
        doc_seq: u64,
        old: Option<Arc<Value>>,
    },
    Delete {
        id: String,
        old: Arc<Value>,
        extracted: HashSet<String>,
        orphaned: Vec<String>,
        seq: u64,
    },
}

#[derive(Clone)]
struct StagedEntry {
    bytes: Arc<[u8]>,
    seq: u64,
}

/// The staged view of one batch: an overlay of not-yet-committed documents,
/// the WAL records they produce, and the RAM mutations waiting for the fsync.
struct Stage<'a> {
    engine: &'a V2Engine,
    overlay: HashMap<Box<str>, Option<StagedEntry>>,
    records: Vec<WalRecord>,
    ops: Vec<AppliedOp>,
    next_seq: u64,
}

impl<'a> Stage<'a> {
    fn new(engine: &'a V2Engine, start_seq: u64) -> Self {
        Stage {
            engine,
            overlay: HashMap::new(),
            records: Vec::new(),
            ops: Vec::new(),
            next_seq: start_seq,
        }
    }

    fn next_seq(&mut self) -> u64 {
        self.next_seq += 1;
        self.next_seq
    }

    fn current_bytes(&self, id: &str) -> Option<Arc<[u8]>> {
        if let Some(entry) = self.overlay.get(id) {
            return entry.as_ref().map(|entry| Arc::clone(&entry.bytes));
        }
        let state = self.engine.state.read();
        state.doc(id).map(|(bytes, _)| Arc::from(bytes))
    }

    fn doc_seq_of(&self, id: &str) -> u64 {
        if let Some(entry) = self.overlay.get(id) {
            return entry.as_ref().map(|entry| entry.seq).unwrap_or(0);
        }
        let state = self.engine.state.read();
        state.doc(id).map(|(_, doc_seq)| doc_seq).unwrap_or(0)
    }

    fn current_value(&self, id: &str) -> Result<Option<Arc<Value>>> {
        match self.current_bytes(id) {
            None => Ok(None),
            Some(bytes) => Ok(Some(Arc::new(parse_doc(&bytes, id)?))),
        }
    }

    fn stage_put(&mut self, id: &str, doc: Value) -> Result<u64> {
        let json = serde_json::to_vec(&doc)?;
        if json.is_empty() {
            return Err(Error::invalid_arg("document serialized to zero bytes"));
        }
        let seq = self.next_seq();
        let bytes: Arc<[u8]> = Arc::from(json.as_slice());
        self.records
            .push(WalRecord::put(seq, id.to_string(), json));
        let old = self.current_value(id)?;
        self.overlay.insert(
            Box::from(id),
            Some(StagedEntry {
                bytes: Arc::clone(&bytes),
                seq,
            }),
        );
        self.ops.push(AppliedOp::Put {
            id: id.to_string(),
            bytes,
            value: Arc::new(doc),
            doc_seq: seq,
            old,
        });
        Ok(seq)
    }

    fn stage_delete(&mut self, id: &str, old: Arc<Value>) -> Result<()> {
        self.engine.ensure_file_refs()?;
        let seq = self.next_seq();
        self.records.push(WalRecord::delete(seq, id.to_string()));
        let mut extracted = HashSet::new();
        self.engine.extract_file_refs(&old, &mut extracted);
        let orphaned = {
            let file_refs = self.engine.file_refs.read();
            extracted
                .iter()
                .filter(|r| file_refs.get(*r).map_or(false, |count| *count <= 1))
                .cloned()
                .collect()
        };
        self.overlay.insert(Box::from(id), None);
        self.ops.push(AppliedOp::Delete {
            id: id.to_string(),
            old,
            extracted,
            orphaned,
            seq,
        });
        Ok(())
    }

    fn run(&mut self, job: Job) -> Result<Reply> {
        match job {
            Job::Insert { mut doc, prefix } => {
                let state = self.engine.state.read();
                let overlay = &self.overlay;
                let check = |candidate: &str| {
                    overlay.contains_key(candidate)
                        || state.doc(candidate).is_some()
                        || state.deleted.contains(candidate)
                };
                let id = match prefix {
                    Some(prefix) => crate::id::generate_checked_with_prefix(&prefix, check),
                    None => crate::id::generate_checked(check),
                };
                drop(state);
                let object = doc
                    .as_object_mut()
                    .ok_or_else(|| Error::invalid_arg("document must be a JSON object"))?;
                object.insert("_id".to_string(), Value::String(id.clone()));
                object.remove(storage::SEQ_FIELD);
                self.stage_put(&id, doc)?;
                Ok(Reply::Id(id))
            }
            Job::Update { id, mut doc, if_seq } => {
                let old = self.current_value(&id)?.ok_or_else(|| Error::not_found(&id))?;
                if let Some(expected) = if_seq {
                    let current = self.doc_seq_of(&id);
                    if expected != current {
                        return Err(Error::Conflict {
                            current_seq: current,
                            doc: (*old).clone(),
                        });
                    }
                }
                let object = doc
                    .as_object_mut()
                    .ok_or_else(|| Error::invalid_arg("document must be a JSON object"))?;
                object.insert("_id".to_string(), Value::String(id.clone()));
                object.remove(storage::SEQ_FIELD);
                self.stage_put(&id, doc)?;
                Ok(Reply::Unit)
            }
            Job::Set { id, path, value } => {
                let mut doc = (*self.current_value(&id)?.ok_or_else(|| Error::not_found(&id))?).clone();
                if !apply_path_set(&mut doc, &path, value) {
                    return Ok(Reply::Applied(false));
                }
                self.stage_put(&id, doc)?;
                Ok(Reply::Applied(true))
            }
            Job::Remove { id, path } => {
                let mut doc = (*self.current_value(&id)?.ok_or_else(|| Error::not_found(&id))?).clone();
                if !apply_path_remove(&mut doc, &path) {
                    return Ok(Reply::Applied(false));
                }
                self.stage_put(&id, doc)?;
                Ok(Reply::Applied(true))
            }
            Job::Push { id, field, value } => {
                let mut doc = (*self.current_value(&id)?.ok_or_else(|| Error::not_found(&id))?).clone();
                if !apply_path_push(&mut doc, &field, value) {
                    return Ok(Reply::Applied(false));
                }
                self.stage_put(&id, doc)?;
                Ok(Reply::Applied(true))
            }
            Job::ApplyOps { id, ops, if_seq } => {
                let old = self.current_value(&id)?.ok_or_else(|| Error::not_found(&id))?;
                if let Some(expected) = if_seq {
                    let current = self.doc_seq_of(&id);
                    if expected != current {
                        return Err(Error::Conflict {
                            current_seq: current,
                            doc: (*old).clone(),
                        });
                    }
                }
                let mut doc = (*old).clone();
                let mut applied = Vec::with_capacity(ops.len());
                for (idx, op) in ops.iter().enumerate() {
                    let ok = match op {
                        PatchOp::Set { path, value } => apply_path_set(&mut doc, path, value.clone()),
                        PatchOp::Remove { path } => apply_path_remove(&mut doc, path),
                        PatchOp::ArrayPush { path, value } => {
                            apply_path_push(&mut doc, path, value.clone())
                        }
                    };
                    if !ok {
                        return Err(Error::invalid_arg(format!(
                            "op {idx} ({}) did not resolve; no changes were applied",
                            match op {
                                PatchOp::Set { path, .. } => format!("set '{path}'"),
                                PatchOp::Remove { path } => format!("remove '{path}'"),
                                PatchOp::ArrayPush { path, .. } => format!("array_push '{path}'"),
                            }
                        )));
                    }
                    applied.push(true);
                }
                let seq = self.stage_put(&id, doc.clone())?;
                Ok(Reply::Ops(ApplyOpsResult { applied, seq, doc }))
            }
            Job::Delete { id } => {
                let old = self.current_value(&id)?.ok_or_else(|| Error::not_found(&id))?;
                self.engine.ensure_file_refs()?;

                let mut extracted = HashSet::new();
                self.engine.extract_file_refs(&old, &mut extracted);
                for r in &extracted {
                    if let Some(file_ref) = crate::FileRef::from_compact(r) {
                        if let Some(policy) = self.engine.bucket_policies.get(&file_ref.bucket) {
                            if policy.on_document_delete == Some(OnDocumentDelete::Restrict) {
                                return Err(Error::policy_violation(format!(
                                    "delete refused: bucket '{}' declares onDocumentDelete=restrict; reassign or release {} first",
                                    file_ref.bucket, r
                                )));
                            }
                        }
                    }
                }

                // C5: the restorable trash record is written and fsynced
                // *before* the tombstone is staged, so a caller told "deleted"
                // can always find the document again.
                if *self.engine.trash_mode.read() != TrashMode::Off {
                    let now = crate::now_secs();
                    let mut trash_doc = (*old).clone();
                    let orphaned: Vec<String> = {
                        let file_refs = self.engine.file_refs.read();
                        extracted
                            .iter()
                            .filter(|r| file_refs.get(*r).map_or(false, |count| *count <= 1))
                            .cloned()
                            .collect()
                    };
                    if let Some(obj) = trash_doc.as_object_mut() {
                        obj.insert("_deleted".to_string(), serde_json::json!(now));
                        if !orphaned.is_empty() {
                            obj.insert(
                                "_trashed_files".to_string(),
                                serde_json::to_value(&orphaned)?,
                            );
                        }
                    }
                    let _trash_guard = self.engine.trash_lock.lock();
                    storage::append_doc_trash(&self.engine.trash_path(), &trash_doc)?;
                }
                self.stage_delete(&id, old)?;
                Ok(Reply::Unit)
            }
            Job::Restore { id, doc } => {
                self.stage_put(&id, doc)?;
                Ok(Reply::Unit)
            }
        }
    }
}

// ─── The engine ─────────────────────────────────────────────────────

/// The live v2 backend of one `Database`.
pub(crate) struct V2Engine {
    dir: PathBuf,
    bucket_policies: HashMap<String, BucketPolicy>,
    trash_mode: RwLock<TrashMode>,
    trash_lock: Arc<Mutex<()>>,
    file_refs: Arc<RwLock<HashMap<String, usize>>>,
    /// `file_refs` is rebuilt from the live document set on first use, not on
    /// open: a cold boot must not parse every document to count file refs.
    file_refs_loaded: std::sync::atomic::AtomicBool,
    state: RwLock<V2State>,
    wal: Mutex<WalState>,
    /// Full in-memory indexes for fields the manifest declares but the base
    /// does not persist (an index created after the last compaction).
    hash_indexes: RwLock<HashMap<String, HashIndex>>,
    btree_indexes: RwLock<HashMap<String, BTreeIndex>>,
    /// Small indexes covering only the documents in `wal_overlay`, for fields
    /// whose base section is persisted. Empty at cold boot.
    wal_hash_indexes: RwLock<HashMap<String, HashIndex>>,
    wal_btree_indexes: RwLock<HashMap<String, BTreeIndex>>,
    text_indexes: RwLock<HashMap<String, TextIndex>>,
    /// Parsed documents, keyed by id and guarded by `doc_seq`.
    parsed_cache: Mutex<HashMap<Box<str>, (u64, Arc<Value>)>>,
    change_ring: RwLock<VecDeque<LoggedChange>>,
    commit_lock: Mutex<()>,
    queue: Mutex<QueueState>,
    queue_cv: Condvar,
    next_job: AtomicU64,
    persistence: RwLock<crate::Persistence>,
}

fn parse_doc(bytes: &[u8], id: &str) -> Result<Value> {
    serde_json::from_slice(bytes).map_err(|e| {
        Error::corruption(
            PathBuf::from(format!("<v2 document {id}>")),
            format!("document bytes are not valid JSON on disk: {e}"),
        )
    })
}

fn io_err<'a>(path: &'a Path, context: &'a str) -> impl FnOnce(std::io::Error) -> Error + 'a {
    Error::io_err(path, context)
}

/// Copy an error so one failed batch fsync can be reported to every waiter
/// without losing the original kind.
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
        Error::Serialization(m) => Error::Serialization(m.clone()),
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
    }
}

impl V2Engine {
    /// Open (or initialise) a v2 database folder.
    ///
    /// The caller has already taken the OS writer lock. On a folder with no
    /// `manifest.json` the v2 artefacts are created from scratch.
    pub(crate) fn open(
        dir: &Path,
        bucket_policies: HashMap<String, BucketPolicy>,
        trash_lock: Arc<Mutex<()>>,
        file_refs: Arc<RwLock<HashMap<String, usize>>>,
    ) -> Result<V2Engine> {
        fs::create_dir_all(dir).map_err(io_err(dir, "create v2 database directory"))?;

        let manifest_path = crate::v2_format::manifest_path(dir);
        let manifest = if manifest_path.exists() {
            let manifest = read_manifest(dir)?;
            warn_leftover_v1(dir)?;
            sweep_stale_generations(dir, &manifest)?;
            manifest
        } else {
            initialise(dir)?
        };

        let base_path = base_file_path(dir, manifest.base_gen);
        let base = MmapBase::open(&base_path).map_err(|e| {
            Error::corruption(
                &base_path,
                format!("cannot map base generation {}: {e}", manifest.base_gen),
            )
        })?;
        if base.base_seq() != manifest.base_seq {
            return Err(Error::corruption(
                &base_path,
                format!(
                    "base carries base_seq {} but the manifest declares {}",
                    base.base_seq(),
                    manifest.base_seq
                ),
            ));
        }
        if base.doc_count() != manifest.doc_count {
            return Err(Error::corruption(
                &base_path,
                format!(
                    "base carries {} documents but the manifest declares {}",
                    base.doc_count(),
                    manifest.doc_count
                ),
            ));
        }
        if base.indexes().len() != manifest.indexes.len() {
            return Err(Error::corruption(
                &base_path,
                format!(
                    "base carries {} indexes but the manifest declares {}",
                    base.indexes().len(),
                    manifest.indexes.len()
                ),
            ));
        }
        let base = Arc::new(base);

        // The base is authoritative for its persisted sections; the manifest
        // must agree with it exactly before any state is published.
        for persisted in base.indexes() {
            let declared = manifest
                .indexes
                .iter()
                .find(|spec| spec.field == persisted.spec.field)
                .ok_or_else(|| {
                    Error::corruption(
                        &base_path,
                        format!(
                            "base carries index {:?} that the manifest does not declare",
                            persisted.spec.field
                        ),
                    )
                })?;
            if declared.kind != persisted.spec.kind {
                return Err(Error::corruption(
                    &base_path,
                    format!(
                        "index {:?} is {:?} in the base but {:?} in the manifest",
                        declared.field, persisted.spec.kind, declared.kind
                    ),
                ));
            }
        }
        for declared in &manifest.indexes {
            let known = base
                .indexes()
                .iter()
                .any(|p| p.spec.field == declared.field);
            if !known {
                return Err(Error::corruption(
                    &base_path,
                    format!(
                        "index {:?} is declared in the manifest but missing from the base",
                        declared.field
                    ),
                ));
            }
        }

        let wal_path = wal_file_path(dir, manifest.wal_gen);
        let replay = replay_wal(&wal_path, manifest.base_seq, true).map_err(|e| {
            Error::corruption(
                &wal_path,
                format!("cannot replay WAL generation {}: {e}", manifest.wal_gen),
            )
        })?;
        if replay.truncated_tail_bytes > 0 {
            report::warning(
                "v2 open",
                format!(
                    "{}: truncated a torn WAL tail ({} byte(s))",
                    wal_path.display(),
                    replay.truncated_tail_bytes
                ),
            );
        }

        let mut state = V2State {
            manifest: manifest.clone(),
            superseded: vec![0u64; (base.slot_count() as usize + 63) / 64],
            suppressed: 0,
            wal_overlay: HashMap::new(),
            base: Arc::clone(&base),
            deleted: HashSet::new(),
            lineage: SourceLineage {
                source_epoch_uuid: manifest.source_epoch_uuid.clone(),
                last_seq: manifest.base_seq,
                min_available_seq: manifest.min_available_seq,
            },
        };
        // Indexes the base persists need only a small overlay index over WAL
        // writes; indexes declared after the last compaction are full heap
        // indexes built over the live document set.
        let mut hash_indexes: HashMap<String, HashIndex> = HashMap::new();
        let mut btree_indexes: HashMap<String, BTreeIndex> = HashMap::new();
        let mut wal_hash_indexes: HashMap<String, HashIndex> = HashMap::new();
        let mut wal_btree_indexes: HashMap<String, BTreeIndex> = HashMap::new();
        // A persisted index still needs its small overlay map, otherwise WAL
        // writes replayed below would have nowhere to record themselves.
        for spec in &manifest.indexes {
            if index_is_persisted(&base, &spec.field) {
                match spec.kind {
                    V2IndexKind::Hash => {
                        wal_hash_indexes.insert(spec.field.clone(), HashIndex::new());
                    }
                    V2IndexKind::BTree => {
                        wal_btree_indexes.insert(spec.field.clone(), BTreeIndex::new());
                    }
                }
            }
        }
        let mut change_ring: VecDeque<LoggedChange> = VecDeque::new();

        for record in &replay.records {
            match record.op {
                WalOp::Put => {
                    let value: Value =
                        serde_json::from_slice(&record.payload).map_err(|e| {
                            Error::corruption(
                                &wal_path,
                                format!(
                                    "WAL frame {} for {:?} is not valid JSON: {e}",
                                    record.seq, record.id
                                ),
                            )
                        })?;
                    let old = state
                        .doc(&record.id)
                        .map(|(bytes, _)| serde_json::from_slice::<Value>(bytes))
                        .transpose()
                        .map_err(|e| {
                            Error::corruption(
                                &wal_path,
                                format!("base document {:?} is not valid JSON: {e}", record.id),
                            )
                        })?;
                    update_indexes_for_doc(
                        &mut state,
                        &mut hash_indexes,
                        &mut btree_indexes,
                        &mut wal_hash_indexes,
                        &mut wal_btree_indexes,
                        &record.id,
                        old.as_ref(),
                        Some(&value),
                    );
                    if let Some(slot) = state.base.find_slot(&record.id) {
                        state.mark_superseded(slot);
                    }
                    state.wal_overlay.insert(
                        Box::from(record.id.as_str()),
                        WalEntry {
                            bytes: Arc::from(record.payload.as_slice()),
                            doc_seq: record.seq,
                        },
                    );
                    state.deleted.remove(&record.id);
                    push_ring(&mut change_ring, record.seq, &record.id, Some(Arc::new(value)));
                }
                WalOp::Delete => {
                    let old = state
                        .doc(&record.id)
                        .map(|(bytes, _)| parse_doc(bytes, &record.id))
                        .transpose()?;
                    if let Some(old) = old.as_ref() {
                        update_indexes_for_doc(
                            &mut state,
                            &mut hash_indexes,
                            &mut btree_indexes,
                            &mut wal_hash_indexes,
                            &mut wal_btree_indexes,
                            &record.id,
                            Some(old),
                            None,
                        );
                    }
                    if let Some(slot) = state.base.find_slot(&record.id) {
                        state.mark_superseded(slot);
                    }
                    state.wal_overlay.remove(record.id.as_str());
                    state.deleted.insert(record.id.clone());
                    push_ring(&mut change_ring, record.seq, &record.id, None);
                }
                WalOp::IndexAdd => {
                    let spec: V2IndexSpec =
                        serde_json::from_slice(&record.payload).map_err(|e| {
                            Error::corruption(
                                &wal_path,
                                format!("WAL index frame is not a V2IndexSpec: {e}"),
                            )
                        })?;
                    declare_index(&mut state.manifest, &spec, true);
                    if !index_is_persisted(&base, &spec.field) {
                        build_index(&mut hash_indexes, &mut btree_indexes, &spec, &state)?;
                    }
                }
                WalOp::IndexDrop => {
                    let spec: V2IndexSpec =
                        serde_json::from_slice(&record.payload).map_err(|e| {
                            Error::corruption(
                                &wal_path,
                                format!("WAL index frame is not a V2IndexSpec: {e}"),
                            )
                        })?;
                    declare_index(&mut state.manifest, &spec, false);
                    hash_indexes.remove(&spec.field);
                    btree_indexes.remove(&spec.field);
                    wal_hash_indexes.remove(&spec.field);
                    wal_btree_indexes.remove(&spec.field);
                }
            }
        }
        state.lineage.last_seq = replay.last_seq;

        let wal = WalWriter::open(&wal_path)?;

        Ok(V2Engine {
            dir: dir.to_path_buf(),
            bucket_policies,
            trash_mode: RwLock::new(TrashMode::Manual),
            trash_lock,
            file_refs,
            file_refs_loaded: std::sync::atomic::AtomicBool::new(false),
            state: RwLock::new(state),
            wal: Mutex::new(WalState {
                primary: wal,
                mirror: None,
            }),
            hash_indexes: RwLock::new(hash_indexes),
            btree_indexes: RwLock::new(btree_indexes),
            wal_hash_indexes: RwLock::new(wal_hash_indexes),
            wal_btree_indexes: RwLock::new(wal_btree_indexes),
            text_indexes: RwLock::new(HashMap::new()),
            parsed_cache: Mutex::new(HashMap::new()),
            change_ring: RwLock::new(change_ring),
            commit_lock: Mutex::new(()),
            queue: Mutex::new(QueueState {
                queue: VecDeque::new(),
                leader_active: false,
            }),
            queue_cv: Condvar::new(),
            next_job: AtomicU64::new(0),
            persistence: RwLock::new(crate::Persistence::Immediate),
        })
    }

    pub(crate) fn set_persistence(&self, mode: crate::Persistence) {
        *self.persistence.write() = mode;
    }

    pub(crate) fn set_trash_mode(&self, mode: TrashMode) {
        *self.trash_mode.write() = mode;
    }

    /// The live manifest (generation numbers, lineage, declared indexes).
    pub(crate) fn manifest(&self) -> V2Manifest {
        self.state.read().manifest.clone()
    }

    fn trash_path(&self) -> PathBuf {
        self.dir.join("_trash").join("docs").join("data.jsonl")
    }

    fn extract_file_refs(&self, value: &Value, out: &mut HashSet<String>) {
        crate::Database::extract_file_refs(value, out);
    }

    // ─── Reads ──────────────────────────────────────────────────────

    /// A parsed document, from the bounded parse cache when the bytes and
    /// commit sequence still match, otherwise parsed once and remembered.
    fn parsed(&self, id: &str, doc_seq: u64, bytes: &[u8]) -> Result<Arc<Value>> {
        if let Some((seq, value)) = self.parsed_cache.lock().get(id) {
            if *seq == doc_seq {
                return Ok(Arc::clone(value));
            }
        }
        let value = Arc::new(parse_doc(bytes, id)?);
        {
            let mut cache = self.parsed_cache.lock();
            // Bounded and stable: once full the cache stops growing rather
            // than clearing, so a corpus larger than the cap never re-parses
            // its whole working set on every round.
            if cache.len() < PARSED_CACHE_CAP {
                cache.insert(Box::from(id), (doc_seq, Arc::clone(&value)));
            } else if let Some(entry) = cache.get_mut(id) {
                *entry = (doc_seq, Arc::clone(&value));
            }
        }
        Ok(value)
    }

    fn read_slot_value(&self, id: &str) -> Result<Option<Arc<Value>>> {
        let state = self.state.read();
        let Some((bytes, doc_seq)) = state.doc(id) else {
            return Ok(None);
        };
        let value = self.parsed(id, doc_seq, bytes)?;
        Ok(Some(value))
    }

    pub(crate) fn get(&self, id: &str) -> Result<Value> {
        match self.read_slot_value(id)? {
            Some(value) => Ok((*value).clone()),
            None => Err(Error::not_found(id)),
        }
    }

    pub(crate) fn doc_seq(&self, id: &str) -> Option<u64> {
        self.state.read().doc(id).map(|(_, doc_seq)| doc_seq)
    }

    pub(crate) fn len(&self) -> usize {
        self.state.read().live_count()
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.len() == 0
    }

    pub(crate) fn contains(&self, id: &str) -> bool {
        self.state.read().doc(id).is_some()
    }

    pub(crate) fn get_all_ids(&self) -> Vec<String> {
        let state = self.state.read();
        let mut ids = Vec::with_capacity(state.live_count());
        for slot in 0..state.slot_count() {
            if state.superseded(slot) {
                continue;
            }
            ids.push(state.base.entry_by_slot(slot).id.to_string());
        }
        ids.extend(state.wal_overlay.keys().map(|id| id.to_string()));
        ids
    }

    pub(crate) fn deleted_ids(&self) -> Vec<String> {
        self.state.read().deleted.iter().cloned().collect()
    }

    pub(crate) fn is_deleted(&self, id: &str) -> bool {
        self.state.read().deleted.contains(id)
    }

    /// Parse every live document into the caller's closure. Used by the
    /// bucket mark phase, which needs the whole set by nature.
    pub(crate) fn for_each_doc<F: FnMut(&str, &Value)>(&self, mut f: F) -> Result<()> {
        let state = self.state.read();
        state.for_each_live(|id, bytes, _| {
            let value = parse_doc(bytes, id)?;
            f(id, &value);
            Ok(())
        })
    }

    pub(crate) fn iter(&self) -> Result<Vec<Value>> {
        let state = self.state.read();
        let mut out = Vec::with_capacity(state.live_count());
        state.for_each_live(|id, bytes, _| {
            out.push(parse_doc(bytes, id)?);
            Ok(())
        })?;
        Ok(out)
    }

    pub(crate) fn all_docs(&self) -> Result<Vec<(String, Value)>> {
        let state = self.state.read();
        let mut out = Vec::with_capacity(state.live_count());
        state.for_each_live(|id, bytes, _| {
            out.push((id.to_string(), parse_doc(bytes, id)?));
            Ok(())
        })?;
        Ok(out)
    }

    /// Rebuild the file-reference counters from the live document set.
    ///
    /// Deferred to first use: counting file refs means parsing every document,
    /// which would put an O(n) parse back into cold boot.
    fn ensure_file_refs(&self) -> Result<()> {
        if self.file_refs_loaded.load(Ordering::SeqCst) {
            return Ok(());
        }
        let mut refs: HashMap<String, usize> = HashMap::new();
        {
            let state = self.state.read();
            state.for_each_live(|id, bytes, _| {
                let value = parse_doc(bytes, id)?;
                let mut found = HashSet::new();
                crate::Database::extract_file_refs(&value, &mut found);
                for r in found {
                    *refs.entry(r).or_insert(0) += 1;
                }
                Ok(())
            })?;
        }
        let mut target = self.file_refs.write();
        target.clear();
        target.extend(refs);
        self.file_refs_loaded.store(true, Ordering::SeqCst);
        Ok(())
    }

    // ─── Query planner (shared types, shared planner) ───────────────

    fn persisted_index<'a>(&self, state: &'a V2State, field: &str) -> Option<&'a PersistedIndex> {
        persisted_index(&state.base, &state.manifest, field)
    }

    fn index_lookup(&self, field: &str, value: &Value) -> Option<HashSet<String>> {
        let state = self.state.read();
        let mut out: HashSet<String> = HashSet::new();
        let mut indexed = false;
        if let Some(index) = self.persisted_index(&state, field) {
            indexed = true;
            for key in ordered_keys(value) {
                if let Some(slots) = state.base.index_slots_for(index, &key) {
                    for slot in slots {
                        if !state.superseded(*slot) {
                            out.insert(state.base.entry_by_slot(*slot).id.to_string());
                        }
                    }
                }
            }
        }
        {
            let index = self.wal_hash_indexes.read();
            if let Some(index) = index.get(field) {
                indexed = true;
                out.extend(index.get(value));
            }
        }
        {
            let index = self.wal_btree_indexes.read();
            if let Some(index) = index.get(field) {
                indexed = true;
                out.extend(index.get(value));
            }
        }
        if !indexed {
            let hash = self.hash_indexes.read();
            if let Some(index) = hash.get(field) {
                indexed = true;
                out.extend(index.get(value));
            }
            drop(hash);
            let btree = self.btree_indexes.read();
            if let Some(index) = btree.get(field) {
                indexed = true;
                out.extend(index.get(value));
            }
        }
        indexed.then_some(out)
    }

    fn index_range(
        &self,
        field: &str,
        min: KeyBound<'_>,
        max: KeyBound<'_>,
    ) -> Option<HashSet<String>> {
        let state = self.state.read();
        let mut out: HashSet<String> = HashSet::new();
        let mut indexed = false;
        if let Some(index) = self.persisted_index(&state, field) {
            if index.spec.kind == V2IndexKind::BTree {
                indexed = true;
                let lower = key_end(min);
                let upper = key_end(max);
                for slot in state.base.index_range_slots(index, lower, upper) {
                    if !state.superseded(slot) {
                        out.insert(state.base.entry_by_slot(slot).id.to_string());
                    }
                }
            }
        }
        {
            let btree = self.wal_btree_indexes.read();
            if let Some(index) = btree.get(field) {
                indexed = true;
                if let Some(ids) = index.range(min, max) {
                    out.extend(ids);
                }
            }
        }
        if !indexed {
            let btree = self.btree_indexes.read();
            if let Some(index) = btree.get(field) {
                indexed = true;
                if let Some(ids) = index.range(min, max) {
                    out.extend(ids);
                }
            }
        }
        indexed.then_some(out)
    }

    fn index_in(&self, field: &str, operand: &Value) -> Option<HashSet<String>> {
        let values = operand.as_array()?;
        if values.is_empty() {
            self.index_lookup(field, &Value::Null)?;
            return Some(HashSet::new());
        }
        let mut out: HashSet<String> = HashSet::new();
        let mut indexed = false;
        for value in values {
            if let Some(ids) = self.index_lookup(field, value) {
                indexed = true;
                out.extend(ids);
            }
        }
        indexed.then_some(out)
    }

    fn plan_candidates(&self, ast: &Value) -> Option<HashSet<String>> {
        crate::plan_candidates(ast, self)
    }

    fn collect_matches(
        &self,
        ast: &Value,
        text_ids: Option<&HashSet<String>>,
    ) -> Result<Vec<Arc<Value>>> {
        let text_ok = |id: &str| text_ids.map_or(true, |set| set.contains(id));
        let candidates = self.plan_candidates(ast);
        let state = self.state.read();
        let mut out = Vec::new();
        match candidates {
            Some(ids) => {
                let mut ids: Vec<&String> = ids.iter().collect();
                ids.sort();
                for id in ids {
                    if !text_ok(id) {
                        continue;
                    }
                    let Some((bytes, doc_seq)) = state.doc(id) else {
                        continue;
                    };
                    if v2_json_scan::matches(bytes, ast) {
                        out.push(self.parsed(id, doc_seq, bytes)?);
                    }
                }
            }
            None => {
                let mut rows: Vec<(String, Arc<[u8]>, u64)> = Vec::new();
                state.for_each_live(|id, bytes, doc_seq| {
                    if text_ok(id) && v2_json_scan::matches(bytes, ast) {
                        rows.push((id.to_string(), Arc::from(bytes), doc_seq));
                    }
                    Ok(())
                })?;
                for (id, bytes, doc_seq) in rows {
                    out.push(self.parsed(&id, doc_seq, &bytes)?);
                }
            }
        }
        Ok(out)
    }

    pub(crate) fn find(&self, field: &str, value: &Value) -> Result<Vec<Value>> {
        let matches = |doc: &Value| doc.get(field).map(|v| values_equal(v, value)).unwrap_or(false);
        if let Some(ids) = self.index_lookup(field, value) {
            let state = self.state.read();
            let mut ids: Vec<String> = ids.into_iter().collect();
            ids.sort();
            let mut out = Vec::new();
            for id in ids {
                let Some((bytes, _)) = state.doc(&id) else {
                    continue;
                };
                let doc = parse_doc(bytes, &id)?;
                if matches(&doc) {
                    out.push(doc);
                }
            }
            return Ok(out);
        }
        let mut out = Vec::new();
        let state = self.state.read();
        state.for_each_live(|id, bytes, _| {
            let doc = parse_doc(bytes, id)?;
            if matches(&doc) {
                out.push(doc);
            }
            Ok(())
        })?;
        Ok(out)
    }

    pub(crate) fn find_where<F>(&self, field: &str, predicate: F) -> Result<Vec<Value>>
    where
        F: Fn(&Value) -> bool,
    {
        let mut out = Vec::new();
        let state = self.state.read();
        state.for_each_live(|id, bytes, _| {
            let doc = parse_doc(bytes, id)?;
            if doc.get(field).map(|v| predicate(v)).unwrap_or(false) {
                out.push(doc);
            }
            Ok(())
        })?;
        Ok(out)
    }

    pub(crate) fn find_range(&self, field: &str, min: &Value, max: &Value) -> Result<Vec<Value>> {
        let in_range = |doc: &Value| {
            doc.get(field)
                .map(|v| {
                    value_cmp(v, min) != std::cmp::Ordering::Less
                        && value_cmp(v, max) != std::cmp::Ordering::Greater
                })
                .unwrap_or(false)
        };
        if let Some(ids) = self.index_range(field, KeyBound::Included(min), KeyBound::Included(max))
        {
            let state = self.state.read();
            let mut ids: Vec<String> = ids.into_iter().collect();
            ids.sort();
            let mut out = Vec::new();
            for id in ids {
                let Some((bytes, _)) = state.doc(&id) else {
                    continue;
                };
                let doc = parse_doc(bytes, &id)?;
                if in_range(&doc) {
                    out.push(doc);
                }
            }
            return Ok(out);
        }
        let mut out = Vec::new();
        let state = self.state.read();
        state.for_each_live(|id, bytes, _| {
            let doc = parse_doc(bytes, id)?;
            if in_range(&doc) {
                out.push(doc);
            }
            Ok(())
        })?;
        Ok(out)
    }

    pub(crate) fn query(&self, ast: Value) -> Result<Vec<Value>> {
        validate_query_ast(&ast)?;
        Ok(self
            .collect_matches(&ast, None)?
            .into_iter()
            .map(|doc| (*doc).clone())
            .collect())
    }

    pub(crate) fn query_with(&self, ast: Value, opts: QueryOptions) -> Result<Vec<Value>> {
        let (_, hits) = self.select_matches(&ast, &opts, None)?;
        let mut out = Vec::with_capacity(hits.len());
        for (id, bytes, doc_seq) in hits {
            out.push((*self.parsed(&id, doc_seq, &bytes)?).clone());
        }
        Ok(out)
    }

    /// Select the matching documents, applying sort/offset/limit *before*
    /// materializing anything.
    ///
    /// The scan filters on raw JSON bytes and extracts only the sort key, so a
    /// rejected document (or one outside the retained page) is never parsed
    /// into a `serde_json::Value`. Returns the pre-pagination match count and
    /// the final page as `(id, bytes, doc_seq)`.
    fn select_matches(
        &self,
        ast: &Value,
        opts: &QueryOptions,
        text_ids: Option<&HashSet<String>>,
    ) -> Result<(usize, Vec<(String, Arc<[u8]>, u64)>)> {
        let text_ok = |id: &str| text_ids.map_or(true, |set| set.contains(id));
        let candidates = self.plan_candidates(ast);
        let state = self.state.read();

        let offset = opts.offset.unwrap_or(0);
        let limit = opts.limit;
        let cap = limit.map(|limit| offset.saturating_add(limit));

        let mut total = 0usize;
        let mut unsorted: Vec<(String, Arc<[u8]>, u64)> = Vec::new();
        let mut sorted: std::collections::BinaryHeap<SortedRow> =
            std::collections::BinaryHeap::new();

        let mut consider = |id: &str, bytes: &[u8], doc_seq: u64| -> Result<()> {
            if !text_ok(id) || !v2_json_scan::matches(bytes, ast) {
                return Ok(());
            }
            let index = total;
            total += 1;
            match &opts.sort_by {
                None => {
                    let keep = cap.map_or(true, |cap| index >= offset && index < cap);
                    if keep {
                        unsorted.push((id.to_string(), Arc::from(bytes), doc_seq));
                    }
                }
                Some((field, dir)) => {
                    let key = v2_json_scan::sort_key(bytes, field).ok_or_else(|| {
                        Error::corruption(
                            PathBuf::from(format!("<v2 document {id}>")),
                            format!(
                                "document bytes are valid JSON but the sort field {field:?} could not be read"
                            ),
                        )
                    })?;
                    let row = SortedRow {
                        key,
                        seq: index,
                        desc: *dir == SortDir::Desc,
                        id: id.to_string(),
                        bytes: Arc::from(bytes),
                        doc_seq,
                    };
                    match cap {
                        None => sorted.push(row),
                        Some(0) => {}
                        Some(cap) => {
                            if sorted.len() < cap {
                                sorted.push(row);
                            } else if let Some(worst) = sorted.peek() {
                                if row < *worst {
                                    sorted.pop();
                                    sorted.push(row);
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        };

        match candidates {
            Some(ids) => {
                let mut ids: Vec<&String> = ids.iter().collect();
                ids.sort();
                // Both the candidate ids and the base directory are sorted by
                // id, so one forward pass resolves every candidate slot — no
                // per-candidate binary search.
                let slots = state.slot_count();
                let mut slot = 0u32;
                for id in ids {
                    if !text_ok(id) {
                        continue;
                    }
                    if state.wal_overlay.contains_key(id.as_str()) {
                        if let Some((bytes, doc_seq)) = state.doc(id) {
                            consider(id, bytes, doc_seq)?;
                        }
                        continue;
                    }
                    while slot < slots
                        && state.base.entry_by_slot(slot).id.as_bytes() < id.as_bytes()
                    {
                        slot += 1;
                    }
                    if slot >= slots {
                        break;
                    }
                    let entry = state.base.entry_by_slot(slot);
                    if entry.id != id.as_str() || state.superseded(slot) {
                        continue;
                    }
                    consider(
                        id,
                        state.base.doc_bytes(entry.data_offset, entry.data_len),
                        entry.doc_seq,
                    )?;
                }
            }
            None => {
                state.for_each_live(|id, bytes, doc_seq| consider(id, bytes, doc_seq))?;
            }
        }

        if opts.sort_by.is_none() {
            return Ok((total, unsorted));
        }
        let ordered = sorted.into_sorted_vec();
        let page = ordered
            .into_iter()
            .skip(offset)
            .take(limit.unwrap_or(usize::MAX))
            .map(|row| (row.id, row.bytes, row.doc_seq))
            .collect();
        Ok((total, page))
    }

    pub(crate) fn query_projected(
        &self,
        ast: Value,
        opts: QueryOptions,
        fields: Option<&[String]>,
        text_ids: Option<&HashSet<String>>,
    ) -> Result<(usize, Vec<Value>)> {
        validate_query_ast(&ast)?;
        let (total, hits) = self.select_matches(&ast, &opts, text_ids)?;
        let mut page = Vec::with_capacity(hits.len());
        for (id, bytes, doc_seq) in hits {
            page.push(match fields {
                Some(fields) => v2_json_scan::project(&bytes, fields),
                None => (*self.parsed(&id, doc_seq, &bytes)?).clone(),
            });
        }
        Ok((total, page))
    }

    pub(crate) fn facet_counts(
        &self,
        fields: &[String],
    ) -> Result<Vec<(String, BTreeMap<String, usize>)>> {
        let mut counts: Vec<(String, BTreeMap<String, usize>)> =
            fields.iter().map(|f| (f.clone(), BTreeMap::new())).collect();
        let state = self.state.read();
        state.for_each_live(|id, bytes, _| {
            let doc = parse_doc(bytes, id)?;
            for (field, map) in counts.iter_mut() {
                let Some(val) = doc.get(field.as_str()) else {
                    continue;
                };
                let terms: Vec<&str> = match val {
                    Value::String(s) => vec![s.as_str()],
                    Value::Array(arr) => arr.iter().filter_map(|x| x.as_str()).collect(),
                    _ => continue,
                };
                for t in terms {
                    *map.entry(t.to_string()).or_insert(0) += 1;
                }
            }
            Ok(())
        })?;
        Ok(counts)
    }

    // ─── Group commit ───────────────────────────────────────────────

    fn submit(&self, job: Job) -> Result<Reply> {
        let id = self.next_job.fetch_add(1, Ordering::SeqCst);
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let mut queue = self.queue.lock();
            queue.queue.push_back(QueuedJob {
                id,
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
                        // Another leader already drained our job.
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
        let _commit = self.commit_lock.lock();

        let start_seq = self.state.read().lineage.last_seq;
        let mut stage = Stage::new(self, start_seq);
        let mut results: Vec<Result<Reply>> = Vec::with_capacity(batch.len());
        for queued in batch {
            let saved_overlay = stage.overlay.clone();
            let saved_records = stage.records.len();
            let saved_ops = stage.ops.len();
            let saved_seq = stage.next_seq;
            match stage.run(job_from_ref(&queued.job)) {
                Ok(reply) => results.push(Ok(reply)),
                Err(e) => {
                    // A failed job stages nothing, and the rest of the batch
                    // continues in FIFO order.
                    stage.overlay = saved_overlay;
                    stage.records.truncate(saved_records);
                    stage.ops.truncate(saved_ops);
                    stage.next_seq = saved_seq;
                    results.push(Err(e));
                }
            }
        }

        if stage.records.is_empty() {
            return results;
        }

        match self.write_records(&stage.records) {
            Ok(()) => {
                self.apply_staged(stage.ops, stage.next_seq);
                results
            }
            Err(error) => {
                // No RAM state moved. Every caller whose job had staged
                // successfully hears the same failure.
                results
                    .into_iter()
                    .map(|result| match result {
                        Ok(_) => Err(duplicate_error(&error)),
                        Err(e) => Err(e),
                    })
                    .collect()
            }
        }
    }

    fn write_records(&self, records: &[WalRecord]) -> Result<()> {
        let immediate = *self.persistence.read() == crate::Persistence::Immediate;
        let mut wal = self.wal.lock();
        let write = |writer: &mut WalWriter| -> Result<()> {
            if immediate {
                writer.append_batch_and_sync(records)
            } else {
                writer.append_batch_no_sync(records)
            }
        };
        write(&mut wal.primary)?;
        if let Some(mirror) = wal.mirror.as_mut() {
            write(mirror)?;
        }
        Ok(())
    }

    fn apply_staged(&self, ops: Vec<AppliedOp>, last_seq: u64) {
        if ops.is_empty() {
            return;
        }

        // Structural state first: a reader that grabs the state lock sees the
        // documents and the watermark move together.
        {
            let mut state = self.state.write();
            for op in &ops {
                match op {
                    AppliedOp::Put {
                        id,
                        bytes,
                        doc_seq,
                        ..
                    } => {
                        if let Some(slot) = state.base.find_slot(id) {
                            state.mark_superseded(slot);
                        }
                        state.wal_overlay.insert(
                            Box::from(id.as_str()),
                            WalEntry {
                                bytes: Arc::clone(bytes),
                                doc_seq: *doc_seq,
                            },
                        );
                        state.deleted.remove(id);
                    }
                    AppliedOp::Delete { id, .. } => {
                        if let Some(slot) = state.base.find_slot(id) {
                            state.mark_superseded(slot);
                        }
                        state.wal_overlay.remove(id.as_str());
                        state.deleted.insert(id.clone());
                        self.parsed_cache.lock().remove(id.as_str());
                    }
                }
            }
            state.lineage.last_seq = last_seq;
        }

        for op in &ops {
            match op {
                AppliedOp::Put {
                    id,
                    value,
                    old,
                    doc_seq,
                    ..
                } => {
                    {
                        let mut hash = self.hash_indexes.write();
                        let mut btree = self.btree_indexes.write();
                        let mut wal_hash = self.wal_hash_indexes.write();
                        let mut wal_btree = self.wal_btree_indexes.write();
                        let state = self.state.read();
                        update_indexes_for_doc(
                            &state,
                            &mut hash,
                            &mut btree,
                            &mut wal_hash,
                            &mut wal_btree,
                            id,
                            old.as_deref(),
                            Some(value),
                        );
                    }
                    self.retext(id, old.as_deref(), Some(value));
                    self.adjust_file_refs(old.as_deref(), Some(value));
                    self.push_change(*doc_seq, id, Some(Arc::clone(value)));
                }
                AppliedOp::Delete {
                    id,
                    old,
                    extracted,
                    orphaned,
                    seq,
                } => {
                    {
                        let mut hash = self.hash_indexes.write();
                        let mut btree = self.btree_indexes.write();
                        let mut wal_hash = self.wal_hash_indexes.write();
                        let mut wal_btree = self.wal_btree_indexes.write();
                        let state = self.state.read();
                        update_indexes_for_doc(
                            &state,
                            &mut hash,
                            &mut btree,
                            &mut wal_hash,
                            &mut wal_btree,
                            id,
                            Some(old),
                            None,
                        );
                    }
                    self.retext(id, Some(old), None);
                    {
                        let mut file_refs = self.file_refs.write();
                        for r in extracted {
                            if let Some(count) = file_refs.get_mut(r) {
                                *count = count.saturating_sub(1);
                                if *count == 0 {
                                    file_refs.remove(r);
                                }
                            }
                        }
                    }
                    for r in orphaned {
                        if let Some(file_ref) = crate::FileRef::from_compact(r) {
                            let bucket = crate::FileBucket::new(&file_ref.bucket, &self.dir);
                            if let Err(e) = bucket.delete(&file_ref) {
                                report::suppressed("v2 delete: move orphaned file to trash", e);
                            }
                        }
                    }
                    self.push_change(*seq, id, None);
                }
            }
        }
    }

    fn push_change(&self, seq: u64, id: &str, value: Option<Arc<Value>>) {
        let mut ring = self.change_ring.write();
        ring.push_back(LoggedChange {
            seq,
            id: id.to_string(),
            kind: match value {
                Some(value) => LoggedKind::Upsert(value),
                None => LoggedKind::Delete,
            },
        });
        while ring.len() > CHANGE_RING_CAP {
            ring.pop_front();
        }
    }

    fn adjust_file_refs(&self, old: Option<&Value>, new: Option<&Value>) {
        let mut old_refs = HashSet::new();
        if let Some(old) = old {
            self.extract_file_refs(old, &mut old_refs);
        }
        let mut new_refs = HashSet::new();
        if let Some(new) = new {
            self.extract_file_refs(new, &mut new_refs);
        }
        let mut refs = self.file_refs.write();
        for r in old_refs.difference(&new_refs) {
            if let Some(count) = refs.get_mut(r) {
                *count = count.saturating_sub(1);
                if *count == 0 {
                    refs.remove(r);
                }
            }
        }
        for r in new_refs.difference(&old_refs) {
            *refs.entry(r.clone()).or_insert(0) += 1;
        }
    }

    fn retext(&self, id: &str, old: Option<&Value>, new: Option<&Value>) {
        let mut text_indexes = self.text_indexes.write();
        if text_indexes.is_empty() {
            return;
        }
        for (field, index) in text_indexes.iter_mut() {
            let had_old = old.map(|d| d.get(field).is_some()).unwrap_or(false);
            let has_new = new.map(|d| d.get(field).is_some()).unwrap_or(false);
            if !had_old && !has_new {
                continue;
            }
            match new {
                Some(doc) => {
                    index.index_doc(id, doc);
                }
                None => index.remove_doc(id),
            }
        }
    }

    // ─── Public mutations ───────────────────────────────────────────

    pub(crate) fn insert(&self, doc: Value, prefix: Option<String>) -> Result<String> {
        match self.submit(Job::Insert { doc, prefix })? {
            Reply::Id(id) => Ok(id),
            other => Err(unexpected_reply("insert", other)),
        }
    }

    pub(crate) fn update(&self, id: &str, doc: Value, if_seq: Option<u64>) -> Result<()> {
        match self.submit(Job::Update {
            id: id.to_string(),
            doc,
            if_seq,
        })? {
            Reply::Unit => Ok(()),
            other => Err(unexpected_reply("update", other)),
        }
    }

    pub(crate) fn set(&self, id: &str, path: &str, value: Value) -> Result<bool> {
        match self.submit(Job::Set {
            id: id.to_string(),
            path: path.to_string(),
            value,
        })? {
            Reply::Applied(applied) => Ok(applied),
            other => Err(unexpected_reply("set", other)),
        }
    }

    pub(crate) fn remove(&self, id: &str, path: &str) -> Result<bool> {
        match self.submit(Job::Remove {
            id: id.to_string(),
            path: path.to_string(),
        })? {
            Reply::Applied(applied) => Ok(applied),
            other => Err(unexpected_reply("remove", other)),
        }
    }

    pub(crate) fn array_push(&self, id: &str, field: &str, value: Value) -> Result<bool> {
        match self.submit(Job::Push {
            id: id.to_string(),
            field: field.to_string(),
            value,
        })? {
            Reply::Applied(applied) => Ok(applied),
            other => Err(unexpected_reply("array_push", other)),
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
            other => Err(unexpected_reply("apply_ops", other)),
        }
    }

    pub(crate) fn delete(&self, id: &str) -> Result<()> {
        match self.submit(Job::Delete { id: id.to_string() })? {
            Reply::Unit => Ok(()),
            other => Err(unexpected_reply("delete", other)),
        }
    }

    pub(crate) fn restore(&self, id: &str, doc: Value) -> Result<()> {
        match self.submit(Job::Restore {
            id: id.to_string(),
            doc,
        })? {
            Reply::Unit => Ok(()),
            other => Err(unexpected_reply("restore", other)),
        }
    }

    // ─── Indexes ────────────────────────────────────────────────────

    pub(crate) fn create_index(&self, field: &str, kind: V2IndexKind) -> Result<()> {
        let _commit = self.commit_lock.lock();
        let spec = V2IndexSpec {
            field: field.to_string(),
            kind,
        };
        {
            let state = self.state.read();
            if state
                .manifest
                .indexes
                .iter()
                .any(|declared| declared.field == field)
            {
                return Err(Error::index_error(field, "index already exists"));
            }
            let mut hash = self.hash_indexes.write();
            let mut btree = self.btree_indexes.write();
            build_index(&mut hash, &mut btree, &spec, &state)?;
            drop(hash);
            drop(btree);
        }
        let seq = self.state.read().lineage.last_seq + 1;
        let record = WalRecord::index_add(seq, &spec)?;
        self.write_records(std::slice::from_ref(&record))?;
        {
            let mut state = self.state.write();
            state.manifest.indexes.push(spec.clone());
            state.lineage.last_seq = seq;
        }
        self.push_change(seq, &spec.field, None);
        Ok(())
    }

    pub(crate) fn drop_index(&self, field: &str) -> Result<()> {
        let _commit = self.commit_lock.lock();
        let spec = {
            let state = self.state.read();
            state
                .manifest
                .indexes
                .iter()
                .find(|declared| declared.field == field)
                .cloned()
                .ok_or_else(|| Error::index_error(field, "index not found"))?
        };
        let seq = self.state.read().lineage.last_seq + 1;
        let record = WalRecord::index_drop(seq, &spec)?;
        self.write_records(std::slice::from_ref(&record))?;
        {
            let mut state = self.state.write();
            state.manifest.indexes.retain(|declared| declared.field != field);
            state.lineage.last_seq = seq;
        }
        self.hash_indexes.write().remove(field);
        self.btree_indexes.write().remove(field);
        self.wal_hash_indexes.write().remove(field);
        self.wal_btree_indexes.write().remove(field);
        self.push_change(seq, field, None);
        Ok(())
    }

    pub(crate) fn has_index(&self, field: &str) -> bool {
        {
            let state = self.state.read();
            if persisted_index(&state.base, &state.manifest, field).is_some() {
                return true;
            }
        }
        self.hash_indexes.read().contains_key(field) || self.btree_indexes.read().contains_key(field)
    }

    // ─── Full-text indexes ──────────────────────────────────────────

    pub(crate) fn has_text_index(&self, field: &str) -> bool {
        self.text_indexes.read().contains_key(field)
    }

    pub(crate) fn create_text_index(&self, field: &str, threads: usize) -> Result<()> {
        let _commit = self.commit_lock.lock();
        let docs_text: Vec<(String, String)> = {
            let state = self.state.read();
            let mut out = Vec::new();
            state.for_each_live(|id, bytes, _| {
                let doc = parse_doc(bytes, id)?;
                if let Some(text) = search::extract_text(&doc, field) {
                    out.push((id.to_string(), text));
                }
                Ok(())
            })?;
            out
        };
        let mut index = TextIndex::new(field);
        index.build_parallel(docs_text, threads);
        self.text_indexes.write().insert(field.to_string(), index);
        Ok(())
    }

    pub(crate) fn drop_text_index(&self, field: &str) -> Result<()> {
        let _commit = self.commit_lock.lock();
        self.text_indexes.write().remove(field);
        Ok(())
    }

    pub(crate) fn text_search(&self, field: &str, search: &TextSearch) -> Result<Vec<String>> {
        search.validate()?;
        let state = self.state.read();
        let text_indexes = self.text_indexes.read();
        let index = text_indexes
            .get(field)
            .ok_or_else(|| Error::invalid_arg(format!("no text index on '{}'", field)))?;
        let mut texts: HashMap<String, String> = HashMap::new();
        state.for_each_live(|id, bytes, _| {
            let doc = parse_doc(bytes, id)?;
            if let Some(text) = search::extract_text(&doc, field) {
                texts.insert(id.to_string(), text);
            }
            Ok(())
        })?;
        Ok(index.search(search, |id| texts.get(id).cloned()))
    }

    // ─── Sync feed ──────────────────────────────────────────────────

    pub(crate) fn lineage(&self) -> SourceLineage {
        self.state.read().lineage.clone()
    }

    pub(crate) fn snapshot_at(&self) -> Result<Snapshot> {
        let _commit = self.commit_lock.lock();
        let state = self.state.read();
        let mut docs = Vec::with_capacity(state.live_count());
        state.for_each_live(|id, bytes, _| {
            docs.push((id.to_string(), parse_doc(bytes, id)?));
            Ok(())
        })?;
        docs.sort_by(|a, b| a.0.cmp(&b.0));
        Ok(Snapshot {
            lineage: state.lineage.clone(),
            docs,
        })
    }

    pub(crate) fn changes_since(&self, since_seq: u64) -> Result<ChangeBatch> {
        let _commit = self.commit_lock.lock();
        let lineage = self.state.read().lineage.clone();
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
            return Err(Error::CursorExpired {
                requested_seq: since_seq,
                min_available_seq: lineage.min_available_seq,
                last_seq: lineage.last_seq,
            });
        }

        // Inside the ring? Answer from RAM.
        let ring_oldest = self.change_ring.read().front().map(|event| event.seq);
        if let Some(oldest) = ring_oldest {
            if since_seq.saturating_add(1) >= oldest {
                let events: Vec<ChangeEvent> = self
                    .change_ring
                    .read()
                    .iter()
                    .filter(|event| event.seq > since_seq)
                    .map(|event| ChangeEvent {
                        seq: event.seq,
                        id: event.id.clone(),
                        kind: match &event.kind {
                            LoggedKind::Upsert(doc) => ChangeKind::Upsert((**doc).clone()),
                            LoggedKind::Delete => ChangeKind::Delete,
                        },
                    })
                    .collect();
                return Ok(ChangeBatch { lineage, events });
            }
        }

        // Older than the ring: stream the WAL from disk.
        let manifest = self.state.read().manifest.clone();
        let wal_path = wal_file_path(&self.dir, manifest.wal_gen);
        let records = scan_wal_since(&wal_path, manifest.base_seq, since_seq)?;
        let mut events = Vec::new();
        for record in records {
            match record.op {
                WalOp::Put => {
                    let doc: Value = serde_json::from_slice(&record.payload).map_err(|e| {
                        Error::corruption(
                            &wal_path,
                            format!("WAL frame {} is not valid JSON: {e}", record.seq),
                        )
                    })?;
                    events.push(ChangeEvent {
                        seq: record.seq,
                        id: record.id,
                        kind: ChangeKind::Upsert(doc),
                    });
                }
                WalOp::Delete => events.push(ChangeEvent {
                    seq: record.seq,
                    id: record.id,
                    kind: ChangeKind::Delete,
                }),
                WalOp::IndexAdd | WalOp::IndexDrop => {}
            }
        }
        Ok(ChangeBatch { lineage, events })
    }

    // ─── Durability ─────────────────────────────────────────────────

    pub(crate) fn flush(&self) -> Result<()> {
        let mut wal = self.wal.lock();
        wal.primary.sync()?;
        if let Some(mirror) = wal.mirror.as_mut() {
            mirror.sync()?;
        }
        Ok(())
    }

    // ─── Compaction ─────────────────────────────────────────────────

    /// Fold the WAL into a new base generation without blocking readers or
    /// writers for the duration of the rewrite.
    ///
    /// The freeze clones the manifest, an `Arc` to the mapping, the superseded
    /// bitset and the small WAL overlay — never the document set. On 100 000
    /// documents that is a few KB instead of 100 000 `Box<str>` slots, which is
    /// what used to stall concurrent writers for ~80 ms.
    pub(crate) fn compact(&self) -> Result<()> {
        // Phase 1 — freeze the commit point and install the mirror WAL.
        let commit = self.commit_lock.lock();
        let (frozen, frozen_seq) = {
            let state = self.state.read();
            let frozen = Frozen {
                manifest: state.manifest.clone(),
                base: Arc::clone(&state.base),
                superseded: state.superseded.clone(),
                overlay: state.wal_overlay.clone(),
                seq: state.lineage.last_seq,
            };
            (frozen, state.lineage.last_seq)
        };
        let frozen_manifest = frozen.manifest.clone();
        let new_base_gen = frozen_manifest.base_gen + 1;
        let new_wal_gen = frozen_manifest.wal_gen + 1;
        let new_wal_path = wal_file_path(&self.dir, new_wal_gen);
        let new_base_path = base_file_path(&self.dir, new_base_gen);
        {
            let mut wal = self.wal.lock();
            if wal.mirror.is_some() {
                return Err(Error::invalid_arg(
                    "a compaction is already in progress on this database",
                ));
            }
            for stale in [&new_base_path, &new_wal_path] {
                if stale.exists() {
                    fs::remove_file(stale)
                        .map_err(io_err(stale, "remove stale compaction file"))?;
                }
            }
            let mirror = WalWriter::create(&new_wal_path, new_wal_gen, frozen_seq)?;
            wal.mirror = Some(mirror);
        }
        drop(commit);

        // Phase 2 — write the new base while everyone keeps working.
        let mut docs: Vec<(String, u64, Arc<[u8]>)> = Vec::new();
        for slot in 0..frozen.base.slot_count() {
            if frozen.is_superseded(slot) {
                continue;
            }
            let entry = frozen.base.entry_by_slot(slot);
            docs.push((
                entry.id.to_string(),
                entry.doc_seq,
                Arc::from(frozen.base.doc_bytes(entry.data_offset, entry.data_len)),
            ));
        }
        docs.extend(frozen.overlay.iter().map(|(id, entry)| {
            (id.to_string(), entry.doc_seq, Arc::clone(&entry.bytes))
        }));
        docs.sort_by(|a, b| a.0.cmp(&b.0));

        // Every declared index is rebuilt from the frozen live set — one raw
        // field read per document per index, never a whole-document parse.
        let frozen_indexes = build_frozen_indexes(&frozen_manifest, &docs)?;

        let inputs: Vec<BaseDocInput<'_>> = docs
            .iter()
            .map(|(id, doc_seq, bytes)| BaseDocInput::new(id, *doc_seq, bytes))
            .collect();
        if let Err(error) = write_base_file(&new_base_path, frozen_seq, &inputs, &frozen_indexes) {
            return self.abort_compaction(&new_base_path, &new_wal_path, error);
        }
        let new_base = match MmapBase::open(&new_base_path) {
            Ok(base) => Arc::new(base),
            Err(error) => {
                return self.abort_compaction(
                    &new_base_path,
                    &new_wal_path,
                    Error::corruption(
                        &new_base_path,
                        format!("compaction produced an unreadable base: {error}"),
                    ),
                )
            }
        };

        // Phase 3 — short critical section: publish the new generation.
        let _commit = self.commit_lock.lock();
        let new_manifest = V2Manifest {
            format_version: frozen_manifest.format_version,
            base_gen: new_base_gen,
            wal_gen: new_wal_gen,
            source_epoch_uuid: frozen_manifest.source_epoch_uuid.clone(),
            base_seq: frozen_seq,
            min_available_seq: frozen_seq + 1,
            doc_count: docs.len() as u64,
            indexes: frozen_manifest.indexes.clone(),
        };
        if let Err(error) = write_manifest_atomic(&self.dir, &new_manifest) {
            return self.abort_compaction(&new_base_path, &new_wal_path, error);
        }

        {
            let mut wal = self.wal.lock();
            let mirror = wal
                .mirror
                .take()
                .ok_or_else(|| Error::invalid_arg("compaction mirror WAL disappeared"))?;
            let primary = std::mem::replace(&mut wal.primary, mirror);
            drop(primary);
        }

        let mut state = self.state.write();
        let mut superseded = vec![0u64; (new_base.slot_count() as usize + 63) / 64];
        let mut suppressed = 0u32;
        // Writes that landed after the freeze stay in the overlay and shadow
        // the base version they raced with.
        let carried = std::mem::take(&mut state.wal_overlay);
        for (id, entry) in carried {
            if entry.doc_seq > frozen_seq {
                if let Some(slot) = new_base.find_slot(&id) {
                    let word = (slot / 64) as usize;
                    let bit = 1u64 << (slot % 64);
                    if superseded[word] & bit == 0 {
                        superseded[word] |= bit;
                        suppressed += 1;
                    }
                }
                state.wal_overlay.insert(id, entry);
            }
        }
        state.base = new_base;
        state.superseded = superseded;
        state.suppressed = suppressed;
        state.manifest = new_manifest.clone();
        state.lineage.min_available_seq = frozen_seq + 1;
        state.lineage.last_seq = state.lineage.last_seq.max(frozen_seq);

        // Rebuild the overlay indexes: everything else now lives in the base.
        let mut wal_hash: HashMap<String, HashIndex> = HashMap::new();
        let mut wal_btree: HashMap<String, BTreeIndex> = HashMap::new();
        for spec in &new_manifest.indexes {
            let mut hash = HashIndex::new();
            let mut btree = BTreeIndex::new();
            for (id, entry) in &state.wal_overlay {
                if let Some(value) = v2_json_scan::field_value(&entry.bytes, &spec.field) {
                    match spec.kind {
                        V2IndexKind::Hash => hash.insert(&value, id),
                        V2IndexKind::BTree => btree.insert(&value, id),
                    }
                }
            }
            match spec.kind {
                V2IndexKind::Hash => {
                    wal_hash.insert(spec.field.clone(), hash);
                }
                V2IndexKind::BTree => {
                    wal_btree.insert(spec.field.clone(), btree);
                }
            }
        }
        {
            self.hash_indexes.write().clear();
            self.btree_indexes.write().clear();
            *self.wal_hash_indexes.write() = wal_hash;
            *self.wal_btree_indexes.write() = wal_btree;
        }
        drop(state);

        {
            let mut ring = self.change_ring.write();
            while ring.front().map_or(false, |event| event.seq <= frozen_seq) {
                ring.pop_front();
            }
        }

        // Old generations are unreferenced now; unlinking them succeeds even
        // while a reader still maps the old base (FILE_SHARE_DELETE).
        let old_base = base_file_path(&self.dir, frozen_manifest.base_gen);
        if old_base != new_base_path {
            if let Err(e) = fs::remove_file(&old_base) {
                report::suppressed("v2 compaction: unlink superseded base", e);
            }
        }
        let old_wal = wal_file_path(&self.dir, frozen_manifest.wal_gen);
        if old_wal != new_wal_path {
            if let Err(e) = fs::remove_file(&old_wal) {
                report::suppressed("v2 compaction: unlink superseded WAL", e);
            }
        }
        Ok(())
    }

    fn abort_compaction(
        &self,
        new_base_path: &Path,
        new_wal_path: &Path,
        error: Error,
    ) -> Result<()> {
        {
            let mut wal = self.wal.lock();
            wal.mirror = None;
        }
        for path in [new_base_path, new_wal_path] {
            if path.exists() {
                if let Err(e) = fs::remove_file(path) {
                    report::suppressed("v2 compaction abort: remove partial file", e);
                }
            }
        }
        Err(error)
    }

    // ─── Export ─────────────────────────────────────────────────────

    /// Write the live document set as `<target>/data.jsonl` with its lineage.
    pub(crate) fn export_snapshot(&self, target: &Path) -> Result<()> {
        let _commit = self.commit_lock.lock();
        let state = self.state.read();
        let mut docs = Vec::with_capacity(state.live_count());
        state.for_each_live(|id, bytes, _| {
            docs.push(parse_doc(bytes, id)?);
            Ok(())
        })?;
        let refs: Vec<&Value> = docs.iter().collect();
        storage::rewrite_atomic_with_meta(
            &target.join("data.jsonl"),
            &refs,
            &SourceLineage {
                source_epoch_uuid: state.lineage.source_epoch_uuid.clone(),
                last_seq: state.lineage.last_seq,
                min_available_seq: state.lineage.last_seq.saturating_add(1),
            },
        )
    }

    /// Human-readable JSONL of every live document (with a `_meta` header when
    /// `with_meta`), for `ndb export --jsonl`.
    pub(crate) fn export_jsonl(&self, out: &Path, with_meta: bool) -> Result<usize> {
        let _commit = self.commit_lock.lock();
        let state = self.state.read();
        let mut buf = String::new();
        if with_meta {
            let meta = serde_json::json!({
                "_meta": {
                    "version": 1,
                    "created": crate::now_secs(),
                    "source_epoch_uuid": state.lineage.source_epoch_uuid,
                    "last_seq": state.lineage.last_seq,
                    "min_available_seq": state.lineage.last_seq.saturating_add(1),
                }
            });
            buf.push_str(&meta.to_string());
            buf.push('\n');
        }
        let mut rows: Vec<(String, Arc<[u8]>)> = Vec::with_capacity(state.live_count());
        state.for_each_live(|id, bytes, _| {
            rows.push((id.to_string(), Arc::from(bytes)));
            Ok(())
        })?;
        rows.sort_by(|a, b| a.0.cmp(&b.0));
        let mut count = 0usize;
        for (id, bytes) in rows {
            let doc = parse_doc(&bytes, &id)?;
            buf.push_str(&serde_json::to_string(&doc)?);
            buf.push('\n');
            count += 1;
        }
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent).map_err(io_err(parent, "create export directory"))?;
            }
        }
        fs::write(out, buf).map_err(io_err(out, "write JSONL export"))?;
        Ok(count)
    }

    /// The raw, pre-serialized JSON bytes of the selected documents.
    ///
    /// The HTTP layer writes these straight into the response body instead of
    /// parsing each hit into a `Value` and re-serializing it.
    pub(crate) fn query_raw(
        &self,
        ast: Value,
        opts: QueryOptions,
        text_ids: Option<&HashSet<String>>,
    ) -> Result<(usize, Vec<Arc<[u8]>>)> {
        validate_query_ast(&ast)?;
        let (total, hits) = self.select_matches(&ast, &opts, text_ids)?;
        Ok((total, hits.into_iter().map(|(_, bytes, _)| bytes).collect()))
    }
}

impl crate::IndexAccess for V2Engine {
    fn lookup(&self, field: &str, value: &Value) -> Option<HashSet<String>> {
        self.index_lookup(field, value)
    }

    fn range_scan(
        &self,
        field: &str,
        min: KeyBound<'_>,
        max: KeyBound<'_>,
    ) -> Option<HashSet<String>> {
        self.index_range(field, min, max)
    }

    fn in_values(&self, field: &str, operand: &Value) -> Option<HashSet<String>> {
        self.index_in(field, operand)
    }
}

fn unexpected_reply(op: &str, reply: Reply) -> Error {
    let kind = match reply {
        Reply::Id(_) => "Id",
        Reply::Applied(_) => "Applied",
        Reply::Ops(_) => "Ops",
        Reply::Unit => "Unit",
    };
    Error::Serialization(format!(
        "v2 group commit returned an unexpected {kind} reply to {op}"
    ))
}

fn job_from_ref(job: &Job) -> Job {
    match job {
        Job::Insert { doc, prefix } => Job::Insert {
            doc: doc.clone(),
            prefix: prefix.clone(),
        },
        Job::Update { id, doc, if_seq } => Job::Update {
            id: id.clone(),
            doc: doc.clone(),
            if_seq: *if_seq,
        },
        Job::Set { id, path, value } => Job::Set {
            id: id.clone(),
            path: path.clone(),
            value: value.clone(),
        },
        Job::Remove { id, path } => Job::Remove {
            id: id.clone(),
            path: path.clone(),
        },
        Job::Push { id, field, value } => Job::Push {
            id: id.clone(),
            field: field.clone(),
            value: value.clone(),
        },
        Job::ApplyOps { id, ops, if_seq } => Job::ApplyOps {
            id: id.clone(),
            ops: ops.clone(),
            if_seq: *if_seq,
        },
        Job::Delete { id } => Job::Delete { id: id.clone() },
        Job::Restore { id, doc } => Job::Restore {
            id: id.clone(),
            doc: doc.clone(),
        },
    }
}

// ─── Index helpers ──────────────────────────────────────────────────

/// The base index for `field`, but only while the runtime manifest still
/// declares it (a WAL `IndexDrop` removes it from the manifest immediately).
fn persisted_index<'a>(
    base: &'a MmapBase,
    manifest: &V2Manifest,
    field: &str,
) -> Option<&'a PersistedIndex> {
    if !manifest.indexes.iter().any(|spec| spec.field == field) {
        return None;
    }
    base.indexes().iter().find(|index| index.spec.field == field)
}

fn index_is_persisted(base: &MmapBase, field: &str) -> bool {
    base.indexes().iter().any(|index| index.spec.field == field)
}

/// The keys one indexed field value contributes, mirroring `index_keys` in
/// `lib.rs`: an array contributes itself and each element.
fn ordered_keys(value: &Value) -> Vec<v2_format::OrderedValue> {
    match value {
        Value::Array(items) if !items.is_empty() => {
            let mut keys = Vec::with_capacity(items.len() + 1);
            keys.push(v2_format::OrderedValue::from_json(value));
            keys.extend(items.iter().map(v2_format::OrderedValue::from_json));
            keys
        }
        _ => vec![v2_format::OrderedValue::from_json(value)],
    }
}

fn key_end(bound: KeyBound<'_>) -> v2_format::KeyEnd {
    match bound {
        KeyBound::Unbounded => v2_format::KeyEnd::Unbounded,
        KeyBound::Included(value) => {
            v2_format::KeyEnd::Included(v2_format::OrderedValue::from_json(value))
        }
        KeyBound::Excluded(value) => {
            v2_format::KeyEnd::Excluded(v2_format::OrderedValue::from_json(value))
        }
    }
}

fn build_index(
    hash: &mut HashMap<String, HashIndex>,
    btree: &mut HashMap<String, BTreeIndex>,
    spec: &V2IndexSpec,
    state: &V2State,
) -> Result<()> {
    let mut hash_index = HashIndex::new();
    let mut btree_index = BTreeIndex::new();
    state.for_each_live(|id, bytes, _| {
        if let Some(value) = v2_json_scan::field_value(bytes, &spec.field) {
            match spec.kind {
                V2IndexKind::Hash => hash_index.insert(&value, id),
                V2IndexKind::BTree => btree_index.insert(&value, id),
            }
        }
        Ok(())
    })?;
    match spec.kind {
        V2IndexKind::Hash => {
            hash.insert(spec.field.clone(), hash_index);
        }
        V2IndexKind::BTree => {
            btree.insert(spec.field.clone(), btree_index);
        }
    }
    Ok(())
}

/// Apply one document mutation to whichever index structure owns each
/// declared field: the small WAL overlay index for a base-persisted field, the
/// full heap index for a field declared after the last compaction.
#[allow(clippy::too_many_arguments)]
fn update_indexes_for_doc(
    state: &V2State,
    heap_hash: &mut HashMap<String, HashIndex>,
    heap_btree: &mut HashMap<String, BTreeIndex>,
    wal_hash: &mut HashMap<String, HashIndex>,
    wal_btree: &mut HashMap<String, BTreeIndex>,
    id: &str,
    old: Option<&Value>,
    new: Option<&Value>,
) {
    for spec in &state.manifest.indexes {
        let persisted = index_is_persisted(&state.base, &spec.field);
        let (hash, btree) = if persisted {
            (&mut *wal_hash, &mut *wal_btree)
        } else {
            (&mut *heap_hash, &mut *heap_btree)
        };
        match spec.kind {
            V2IndexKind::Hash => {
                if let Some(index) = hash.get_mut(&spec.field) {
                    if let Some(value) = old.and_then(|doc| doc.get(spec.field.as_str())) {
                        index.remove(value, id);
                    }
                    if let Some(value) = new.and_then(|doc| doc.get(spec.field.as_str())) {
                        index.insert(value, id);
                    }
                }
            }
            V2IndexKind::BTree => {
                if let Some(index) = btree.get_mut(&spec.field) {
                    if let Some(value) = old.and_then(|doc| doc.get(spec.field.as_str())) {
                        index.remove(value, id);
                    }
                    if let Some(value) = new.and_then(|doc| doc.get(spec.field.as_str())) {
                        index.insert(value, id);
                    }
                }
            }
        }
    }
}

/// Rebuild every declared index over the frozen live document set.
fn build_frozen_indexes(
    manifest: &V2Manifest,
    docs: &[(String, u64, Arc<[u8]>)],
) -> Result<Vec<(V2IndexSpec, Vec<BaseIndexEntry>)>> {
    let mut out = Vec::with_capacity(manifest.indexes.len());
    for spec in &manifest.indexes {
        let mut hash = HashIndex::new();
        let mut btree = BTreeIndex::new();
        for (id, _, bytes) in docs {
            if let Some(value) = v2_json_scan::field_value(bytes, &spec.field) {
                match spec.kind {
                    V2IndexKind::Hash => hash.insert(&value, id),
                    V2IndexKind::BTree => btree.insert(&value, id),
                }
            }
        }
        let entries = match spec.kind {
            V2IndexKind::Hash => hash.entries(),
            V2IndexKind::BTree => btree.entries(),
        };
        out.push((spec.clone(), entries));
    }
    Ok(out)
}

/// The immutable read view compaction freezes before it starts writing.
struct Frozen {
    manifest: V2Manifest,
    base: Arc<MmapBase>,
    superseded: Vec<u64>,
    overlay: HashMap<Box<str>, WalEntry>,
    #[allow(dead_code)]
    seq: u64,
}

impl Frozen {
    fn is_superseded(&self, slot: u32) -> bool {
        let word = (slot / 64) as usize;
        let bit = 1u64 << (slot % 64);
        self.superseded.get(word).map_or(false, |value| value & bit != 0)
    }
}

/// One result row retained by a sorted query, ordered exactly the way the
/// output page is ordered (so a bounded max-heap keeps the smallest `cap`).
struct SortedRow {
    key: v2_format::OrderedValue,
    seq: usize,
    desc: bool,
    id: String,
    bytes: Arc<[u8]>,
    doc_seq: u64,
}

impl PartialEq for SortedRow {
    fn eq(&self, other: &Self) -> bool {
        self.cmp(other) == std::cmp::Ordering::Equal
    }
}

impl Eq for SortedRow {}

impl PartialOrd for SortedRow {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for SortedRow {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        let ordering = self.key.cmp(&other.key);
        let ordering = if self.desc {
            ordering.reverse()
        } else {
            ordering
        };
        ordering.then(self.seq.cmp(&other.seq))
    }
}

fn declare_index(manifest: &mut V2Manifest, spec: &V2IndexSpec, add: bool) {
    manifest.indexes.retain(|declared| declared.field != spec.field);
    if add {
        manifest.indexes.push(spec.clone());
    }
}

fn push_ring(ring: &mut VecDeque<LoggedChange>, seq: u64, id: &str, value: Option<Arc<Value>>) {
    ring.push_back(LoggedChange {
        seq,
        id: id.to_string(),
        kind: match value {
            Some(value) => LoggedKind::Upsert(value),
            None => LoggedKind::Delete,
        },
    });
    while ring.len() > CHANGE_RING_CAP {
        ring.pop_front();
    }
}

// ─── Folder initialisation / hygiene ────────────────────────────────

/// Create the artefacts of a brand-new v2 folder.
pub(crate) fn initialise(dir: &Path) -> Result<V2Manifest> {
    let manifest = V2Manifest {
        format_version: crate::v2_format::STORAGE_VERSION,
        base_gen: 1,
        wal_gen: 1,
        source_epoch_uuid: storage::generate_epoch_uuid(),
        base_seq: 0,
        min_available_seq: 1,
        doc_count: 0,
        indexes: Vec::new(),
    };

    let base_path = base_file_path(dir, manifest.base_gen);
    if base_path.exists() {
        fs::remove_file(&base_path).map_err(io_err(&base_path, "remove orphan base"))?;
    }
    write_base_file(&base_path, 0, &[], &[])?;

    let wal_path = wal_file_path(dir, manifest.wal_gen);
    if wal_path.exists() {
        fs::remove_file(&wal_path).map_err(io_err(&wal_path, "remove orphan WAL"))?;
    }
    WalWriter::create(&wal_path, manifest.wal_gen, 0)?;

    write_manifest_atomic(dir, &manifest)?;
    Ok(manifest)
}

/// A v2 folder may still hold v1 leftovers (`data.jsonl`, `_index_*.fti`); they
/// are ignored, but never silently.
fn warn_leftover_v1(dir: &Path) -> Result<()> {
    let journal = dir.join("data.jsonl");
    if journal.exists() {
        report::warning(
            "v2 open",
            format!(
                "{}: ignoring leftover v1 journal (the manifest is authoritative)",
                journal.display()
            ),
        );
    }
    for entry in fs::read_dir(dir).map_err(io_err(dir, "list v2 database folder"))? {
        let entry = entry.map_err(io_err(dir, "read v2 folder entry"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        if name.starts_with("_index_") && name.ends_with(".fti") {
            report::warning(
                "v2 open",
                format!("ignoring stale v1 full-text cache {}", entry.path().display()),
            );
        }
    }
    Ok(())
}

fn generation_of(name: &str, prefix: &str, suffix: &str) -> Option<u64> {
    let rest = name.strip_prefix(prefix)?.strip_suffix(suffix)?;
    rest.parse().ok()
}

/// Remove `base.gen_*` / `wal.gen_*` files the manifest no longer references.
fn sweep_stale_generations(dir: &Path, manifest: &V2Manifest) -> Result<()> {
    for entry in fs::read_dir(dir).map_err(io_err(dir, "list v2 database folder"))? {
        let entry = entry.map_err(io_err(dir, "read v2 folder entry"))?;
        let name = entry.file_name().to_string_lossy().to_string();
        let stale = |prefix: &str, current: u64| -> bool {
            matches!(generation_of(&name, prefix, ".ndb"), Some(gen) if gen != current)
        };
        if stale("base.gen_", manifest.base_gen) || stale("wal.gen_", manifest.wal_gen) {
            if let Err(e) = fs::remove_file(entry.path()) {
                report::suppressed("v2 open: sweep stale generation", e);
            }
        }
    }
    Ok(())
}
