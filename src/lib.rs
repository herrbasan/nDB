//! ndb - Human-readable document database for the AI age.
//!
//! Part of the nGDB platform ecosystem. In-memory document store with
//! JSON Lines persistence, layered query API, and file bucket support.
//!
//! # Architecture
//!
//! - **Layer 1 (Core):** O(1) insert/get/update/delete via HashMap
//! - **Layer 2 (Field Queries):** Single-field equality/predicate queries
//! - **Layer 3 (JSON AST):** Complex queries via raw JSON AST
//! - **File Buckets:** Binary storage with hash-based deduplication
//!
//! # Example
//!
//! ```no_run
//! use ndb::Database;
//! use serde_json::json;
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let db = Database::open("data.jsonl")?;
//! let id = db.insert(json!({"title": "Hello"}))?;
//! let doc = db.get(&id)?;
//! println!("{}", doc);
//! # Ok(())
//! # }
//! ```

pub mod bucket;
pub mod crc32;
pub mod error;
pub mod id;
pub mod item;
pub mod mmap;
pub mod report;
pub mod search;
pub mod server;
pub mod storage;
pub mod v2_format;
mod v2_engine;
mod v2_json_scan;

pub use v2_engine::CHANGE_RING_CAP as V2_CHANGE_RING_CAP;

pub use bucket::{FileBucket, FileMeta, FileRef};
pub use crc32::{crc32, Crc32Hasher};
pub use error::{Error, Result};
pub use item::{ItemBucket, ItemRecord, ItemState};
pub use mmap::MmapFile;
pub use search::{TextMode, TextQuery, TextSearch};
pub use storage::{ChangeBatch, ChangeEvent, ChangeKind, Snapshot, SourceLineage};
pub use v2_format::{
    base_file_name, base_file_path, fsck_v2_dir, manifest_path, read_manifest, repair_wal_file,
    replay_wal, scan_wal_since, wal_file_name, wal_file_path, write_base_file,
    write_manifest_atomic, BaseDirEntry, BaseDocInput, BaseHeader, BaseIndexEntry, FsckReport,
    MmapBase, PersistedIndex, V2IndexKind, V2IndexSpec, V2Manifest, WalOp,
    WalRecord, WalRepairStats, WalReplay, WalWriter, MANIFEST_FILE, STORAGE_VERSION,
};

use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use id::{generate_checked, generate_checked_with_prefix};

// ─── Persistence Modes ──────────────────────────────────────────────

/// When to persist data to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    /// Flush on explicit call or shutdown. Fastest, last flush only.
    Lazy,
    /// Fsync on a background timer at this interval. Balanced: a crash can
    /// lose at most one interval of writes, and the flush really runs (H2).
    Scheduled(Duration),
    /// fsync after every write. Slowest, every write safe.
    Immediate,
}

impl Default for Persistence {
    fn default() -> Self {
        Persistence::Lazy
    }
}

// ─── Storage Format ─────────────────────────────────────────────────

/// Which on-disk layout a database uses.
///
/// `Auto` is the default: a folder holding `manifest.json` opens on the v2
/// binary/mmap backend, a folder holding `data.jsonl` opens on the v1 JSONL
/// backend, and a brand-new folder initialises as v1 (backward compatible).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum StorageFormat {
    /// Detect from the folder contents.
    #[default]
    Auto,
    /// JSON Lines journal (`data.jsonl`).
    V1,
    /// Binary base generations + CRC-framed WAL.
    V2,
}

/// How a database is opened.
#[derive(Debug, Clone, Default)]
pub struct OpenOptions {
    /// Storage format; `Auto` detects from the folder.
    pub format: StorageFormat,
    /// Persistence mode applied to the opened handle.
    pub persistence: Persistence,
}

impl StorageFormat {
    /// Parse `auto` / `v1` / `v2` (case-insensitive). Unknown values fail.
    pub fn parse(text: &str) -> Result<Self> {
        match text.to_ascii_lowercase().as_str() {
            "auto" => Ok(StorageFormat::Auto),
            "v1" => Ok(StorageFormat::V1),
            "v2" => Ok(StorageFormat::V2),
            other => Err(Error::invalid_arg(format!(
                "format must be auto | v1 | v2, got '{other}'"
            ))),
        }
    }
}

// ─── Trash Mode ─────────────────────────────────────────────────────

/// How to handle trashed documents/files.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrashMode {
    /// Never auto-delete (default).
    Manual,
    /// Auto-purge after given duration.
    TTL(Duration),
    /// Hard delete immediately (dangerous).
    Off,
}

impl Default for TrashMode {
    fn default() -> Self {
        TrashMode::Manual
    }
}

// ─── Query Types ────────────────────────────────────────────────────

/// Sort direction for query results.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SortDir {
    Asc,
    Desc,
}

/// Options for query_with.
#[derive(Debug, Clone, Default)]
pub struct QueryOptions {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub sort_by: Option<(String, SortDir)>,
}

// ─── Transactional Patches ──────────────────────────────────────────

/// One mutation inside a transactional [`Database::apply_ops`] batch.
#[derive(Debug, Clone, PartialEq)]
pub enum PatchOp {
    /// Assign `value` at a dot-separated path.
    Set { path: String, value: Value },
    /// Remove the field or array element at a dot-separated path.
    Remove { path: String },
    /// Append `value` to the array at a dot-separated path.
    ArrayPush { path: String, value: Value },
}

/// Outcome of an applied batch.
#[derive(Debug, Clone)]
pub struct ApplyOpsResult {
    /// One entry per op. Always `true`: an op that does not resolve aborts the
    /// whole batch instead of producing a `false`.
    pub applied: Vec<bool>,
    /// Commit sequence of the single journal record.
    pub seq: u64,
    /// The document's post-mutation state.
    pub doc: Value,
}

// ─── Index Types ────────────────────────────────────────────────────

// ─── Ordered Index Keys ─────────────────────────────────────────────

/// Total order over JSON values, shared by [`value_cmp`], the BTree index and
/// every range query. One ordering means an indexed range returns exactly the
/// set a linear scan returns — numbers compare numerically (`9 < 10`,
/// `-5 < 2`), never as their decimal text, and arrays/objects sort after every
/// scalar by canonical JSON.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub(crate) enum OrderedValue {
    Null,
    Bool(bool),
    Number(OrderedF64),
    String(String),
    /// Arrays and objects: compared by their canonical JSON text.
    Composite(String),
}

/// `f64` with a total order (and a matching `Hash`), so `-0.0 < 0.0` and
/// every Number key is usable in both a `HashMap` and a `BTreeMap`.
#[derive(Clone, Copy, Debug)]
struct OrderedF64(f64);

impl PartialEq for OrderedF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == std::cmp::Ordering::Equal
    }
}

impl Eq for OrderedF64 {}

impl PartialOrd for OrderedF64 {
    fn partial_cmp(&self, other: &Self) -> Option<std::cmp::Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for OrderedF64 {
    fn cmp(&self, other: &Self) -> std::cmp::Ordering {
        self.0.total_cmp(&other.0)
    }
}

impl std::hash::Hash for OrderedF64 {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.0.to_bits().hash(state);
    }
}

fn ordered_value(value: &Value) -> OrderedValue {
    match value {
        Value::Null => OrderedValue::Null,
        Value::Bool(b) => OrderedValue::Bool(*b),
        Value::Number(n) => OrderedValue::Number(OrderedF64(
            n.as_f64()
                .expect("serde_json number is not representable as f64"),
        )),
        Value::String(s) => OrderedValue::String(s.clone()),
        other => OrderedValue::Composite(other.to_string()),
    }
}

impl OrderedValue {
    /// Convert to the on-disk `v2_format` key type used by base index sections.
    pub(crate) fn to_v2(&self) -> v2_format::OrderedValue {
        match self {
            OrderedValue::Null => v2_format::OrderedValue::Null,
            OrderedValue::Bool(b) => v2_format::OrderedValue::Bool(*b),
            OrderedValue::Number(n) => v2_format::OrderedValue::Number(n.0),
            OrderedValue::String(s) => v2_format::OrderedValue::String(s.clone()),
            OrderedValue::Composite(s) => v2_format::OrderedValue::Composite(s.clone()),
        }
    }
}

/// The keys one indexed field value contributes.
///
/// An array contributes its whole value **and** each element, exactly the way
/// [`evaluate_condition`] reads it (`$eq` on the whole value, `$in`/range
/// against elements). Without the element keys an index lookup would miss
/// documents a linear scan matches — the planner relies on this being a
/// superset of `query_matches`.
fn index_keys(value: &Value) -> Vec<OrderedValue> {
    match value {
        Value::Array(items) if !items.is_empty() => {
            let mut keys = Vec::with_capacity(items.len() + 1);
            keys.push(ordered_value(value));
            keys.extend(items.iter().map(ordered_value));
            keys
        }
        _ => vec![ordered_value(value)],
    }
}

/// One end of an indexed range scan.
#[derive(Clone, Copy)]
pub(crate) enum KeyBound<'a> {
    Unbounded,
    Included(&'a Value),
    Excluded(&'a Value),
}

impl KeyBound<'_> {
    fn ordered(self) -> std::ops::Bound<OrderedValue> {
        match self {
            KeyBound::Unbounded => std::ops::Bound::Unbounded,
            KeyBound::Included(v) => std::ops::Bound::Included(ordered_value(v)),
            KeyBound::Excluded(v) => std::ops::Bound::Excluded(ordered_value(v)),
        }
    }
}

/// Trait for secondary indexes.
pub(crate) trait Index: Send + Sync {
    fn insert(&mut self, value: &Value, id: &str);
    fn remove(&mut self, value: &Value, id: &str);
    fn get(&self, value: &Value) -> Vec<String>;
    /// Ordered range over the key space. `None` when the index has no order
    /// (hash) — the planner must not treat a hash index as range-capable.
    fn range(&self, min: KeyBound<'_>, max: KeyBound<'_>) -> Option<Vec<String>>;
}

/// Hash index for O(1) equality lookups.
pub(crate) struct HashIndex {
    map: HashMap<OrderedValue, HashSet<String>>,
}

impl HashIndex {
    pub(crate) fn new() -> Self {
        HashIndex {
            map: HashMap::new(),
        }
    }

    /// The map as persisted-base index entries, sorted by key.
    pub(crate) fn entries(&self) -> Vec<BaseIndexEntry> {
        let mut out: Vec<(OrderedValue, Vec<String>)> = self
            .map
            .iter()
            .map(|(key, ids)| {
                let mut ids: Vec<String> = ids.iter().cloned().collect();
                ids.sort();
                (key.clone(), ids)
            })
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        out.into_iter()
            .map(|(key, ids)| BaseIndexEntry::new(key.to_v2(), ids))
            .collect()
    }
}

impl Index for HashIndex {
    fn insert(&mut self, value: &Value, id: &str) {
        for key in index_keys(value) {
            self.map.entry(key).or_default().insert(id.to_string());
        }
    }

    fn remove(&mut self, value: &Value, id: &str) {
        for key in index_keys(value) {
            if let Some(set) = self.map.get_mut(&key) {
                set.remove(id);
                if set.is_empty() {
                    self.map.remove(&key);
                }
            }
        }
    }

    fn get(&self, value: &Value) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<&String> = HashSet::new();
        for key in index_keys(value) {
            if let Some(set) = self.map.get(&key) {
                for id in set {
                    if seen.insert(id) {
                        out.push(id.clone());
                    }
                }
            }
        }
        out
    }

    fn range(&self, _min: KeyBound<'_>, _max: KeyBound<'_>) -> Option<Vec<String>> {
        None
    }
}

/// BTree index for O(log n) lookups + exact ordered range scans.
pub(crate) struct BTreeIndex {
    map: BTreeMap<OrderedValue, HashSet<String>>,
}

impl BTreeIndex {
    pub(crate) fn new() -> Self {
        BTreeIndex {
            map: BTreeMap::new(),
        }
    }

    /// The map as persisted-base index entries, in key order.
    pub(crate) fn entries(&self) -> Vec<BaseIndexEntry> {
        self.map
            .iter()
            .map(|(key, ids)| {
                let mut ids: Vec<String> = ids.iter().cloned().collect();
                ids.sort();
                BaseIndexEntry::new(key.to_v2(), ids)
            })
            .collect()
    }
}

impl Index for BTreeIndex {
    fn insert(&mut self, value: &Value, id: &str) {
        for key in index_keys(value) {
            self.map.entry(key).or_default().insert(id.to_string());
        }
    }

    fn remove(&mut self, value: &Value, id: &str) {
        for key in index_keys(value) {
            if let Some(set) = self.map.get_mut(&key) {
                set.remove(id);
                if set.is_empty() {
                    self.map.remove(&key);
                }
            }
        }
    }

    fn get(&self, value: &Value) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<&String> = HashSet::new();
        for key in index_keys(value) {
            if let Some(set) = self.map.get(&key) {
                for id in set {
                    if seen.insert(id) {
                        out.push(id.clone());
                    }
                }
            }
        }
        out
    }

    fn range(&self, min: KeyBound<'_>, max: KeyBound<'_>) -> Option<Vec<String>> {
        let mut out: Vec<String> = Vec::new();
        let mut seen: HashSet<&String> = HashSet::new();
        for (_, ids) in self.map.range((min.ordered(), max.ordered())) {
            for id in ids {
                if seen.insert(id) {
                    out.push(id.clone());
                }
            }
        }
        Some(out)
    }
}

// ─── Query Evaluator ────────────────────────────────────────────────

const KNOWN_OPERATORS: [&str; 9] = [
    "$eq", "$ne", "$gt", "$gte", "$lt", "$lte", "$in", "$nin", "$exists",
];

/// Validate a query AST. Unknown operators, malformed combinators, or a
/// non-object/non-array root return Err instead of silently matching
/// everything. The HTTP server calls this on every inbound query — a client
/// typo ($eqq) must fail loud (400), never degrade to a full-DB scan.
pub fn validate_query_ast(ast: &Value) -> Result<()> {
    match ast {
        Value::Object(map) => {
            for (key, val) in map {
                match key.as_str() {
                    "$and" | "$or" => {
                        let arr = val
                            .as_array()
                            .ok_or_else(|| Error::invalid_arg(format!("{} requires an array", key)))?;
                        if arr.is_empty() {
                            return Err(Error::invalid_arg(format!("{} requires a non-empty array", key)));
                        }
                        for cond in arr {
                            validate_query_ast(cond)?;
                        }
                    }
                    "$not" => validate_query_ast(val)?,
                    op if op.starts_with('$') => {
                        return Err(Error::invalid_arg(format!("unknown operator {}", op)));
                    }
                    _ => {
                        // Field condition: value is either an operator object or an implicit $eq.
                        // Every key in an operator object must be a known operator — this makes
                        // validation and evaluation agree (evaluate_condition's catch-all
                        // `_ => true` is unreachable for validated ASTs). Consequence: an
                        // object value can no longer be matched via implicit $eq on nested
                        // keys ({"field": {"a": 1}} now 400s as "unknown operator a").
                        if let Value::Object(op_map) = val {
                            for op in op_map.keys() {
                                if !KNOWN_OPERATORS.contains(&op.as_str()) {
                                    return Err(Error::invalid_arg(format!("unknown operator {}", op)));
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        Value::Array(conditions) => {
            for cond in conditions {
                validate_query_ast(cond)?;
            }
            Ok(())
        }
        _ => Err(Error::invalid_arg("query AST must be an object or array")),
    }
}

/// Build a projection of a document: only the requested dot-paths plus _id.
pub(crate) fn project_doc(doc: &Value, fields: &[String]) -> Value {
    let mut out = serde_json::Map::new();
    for f in fields {
        if let Some(v) = field_get(doc, f) {
            out.insert(f.clone(), v.clone());
        }
    }
    if let Some(id) = doc.get("_id") {
        out.insert("_id".to_string(), id.clone());
    }
    Value::Object(out)
}

/// Evaluate a JSON AST query against a document.
pub(crate) fn query_matches(doc: &Value, ast: &Value) -> bool {
    match ast {
        Value::Object(map) => {
            // Check for logical combinators first
            if let Some(and_expr) = map.get("$and") {
                return and_expr
                    .as_array()
                    .map(|arr| arr.iter().all(|cond| query_matches(doc, cond)))
                    .unwrap_or(false);
            }
            if let Some(or_expr) = map.get("$or") {
                return or_expr
                    .as_array()
                    .map(|arr| arr.iter().any(|cond| query_matches(doc, cond)))
                    .unwrap_or(false);
            }
            if let Some(not_expr) = map.get("$not") {
                return !query_matches(doc, not_expr);
            }

            // Field conditions: {"field": {"$op": value}} or {"field": value} (implicit $eq)
            map.iter().all(|(field, condition)| {
                let field_val = field_get(doc, field);
                match field_val {
                    None => {
                        // $exists: false should match when field is missing
                        if let Value::Object(op_map) = condition {
                            if let Some(exists_val) = op_map.get("$exists") {
                                return !exists_val.as_bool().unwrap_or(true);
                            }
                        }
                        false
                    }
                    Some(val) => evaluate_condition(val, condition),
                }
            })
        }
        Value::Array(conditions) => {
            // Array at top level = implicit $and
            conditions.iter().all(|cond| query_matches(doc, cond))
        }
        _ => false,
    }
}

/// Get a field value from a document. Supports dot notation.
pub(crate) fn field_get<'a>(doc: &'a Value, field: &str) -> Option<&'a Value> {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = doc;
    for part in parts {
        // Array-aware path resolution: numeric segments index into arrays
        // (same capability as the mutation walker in walk_and_set), so
        // "messages.0.role" resolves for reads too.
        if let Value::Array(arr) = current {
            let idx = part.parse::<usize>().ok()?;
            current = arr.get(idx)?;
        } else {
            current = current.get(part)?;
        }
    }
    Some(current)
}

/// Evaluate a condition against a field value.
fn evaluate_condition(field_val: &Value, condition: &Value) -> bool {
    match condition {
        Value::Object(op_map) => {
            // Operator-based: {"$eq": "value", "$gt": 10, ...}
            op_map.iter().all(|(op, operand)| match op.as_str() {
                "$eq" => values_equal(field_val, operand),
                "$ne" => !values_equal(field_val, operand),
                "$gt" => value_cmp(field_val, operand) == std::cmp::Ordering::Greater,
                "$gte" => value_cmp(field_val, operand) != std::cmp::Ordering::Less,
                "$lt" => value_cmp(field_val, operand) == std::cmp::Ordering::Less,
                "$lte" => value_cmp(field_val, operand) != std::cmp::Ordering::Greater,
                // Array-aware: if the field value is an array, match elements
                // ({"tags": {"$in": ["a"]}} finds docs whose tags contain "a").
                // Scalar field values keep whole-value comparison.
                "$in" => operand
                    .as_array()
                    .map(|arr| {
                        if let Value::Array(field_arr) = field_val {
                            field_arr.iter().any(|el| arr.iter().any(|v| values_equal(el, v)))
                        } else {
                            arr.iter().any(|v| values_equal(field_val, v))
                        }
                    })
                    .unwrap_or(false),
                "$nin" => operand
                    .as_array()
                    .map(|arr| {
                        let hit = if let Value::Array(field_arr) = field_val {
                            field_arr.iter().any(|el| arr.iter().any(|v| values_equal(el, v)))
                        } else {
                            arr.iter().any(|v| values_equal(field_val, v))
                        };
                        !hit
                    })
                    .unwrap_or(true),
                "$exists" => operand.as_bool().unwrap_or(true),
                // Unknown op = no filter. Only reachable on unvalidated ASTs
                // (napi/CLI query path) — the HTTP route runs validate_query_ast
                // first, so every key is known there.
                _ => true,
            })
        }
        // Implicit $eq: {"field": "value"}
        _ => values_equal(field_val, condition),
    }
}

/// Compare two JSON values for equality.
pub(crate) fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(an), Value::Number(bn)) => {
            // Compare as f64 for cross-type number equality
            an.as_f64() == bn.as_f64()
        }
        _ => a == b,
    }
}

/// Compare two JSON values for ordering.
///
/// Uses the same total order the BTree index is keyed by, so a range scan and
/// a linear scan agree on every pair — including mixed types, where the order
/// is Null < Bool < Number < String < array/object.
pub(crate) fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    ordered_value(a).cmp(&ordered_value(b))
}

/// Tightest lower bound: the greatest value wins; on a tie the exclusive bound
/// wins (it admits less).
pub(crate) fn stronger_lower(current: Option<(Value, bool)>, value: &Value, inclusive: bool) -> Option<(Value, bool)> {
    match current {
        None => Some((value.clone(), inclusive)),
        Some((bound, bound_inclusive)) => match value_cmp(&bound, value) {
            std::cmp::Ordering::Less => Some((value.clone(), inclusive)),
            std::cmp::Ordering::Equal if bound_inclusive && !inclusive => {
                Some((value.clone(), inclusive))
            }
            _ => Some((bound, bound_inclusive)),
        },
    }
}

/// Tightest upper bound: the smallest value wins; on a tie the exclusive bound
/// wins.
pub(crate) fn stronger_upper(current: Option<(Value, bool)>, value: &Value, inclusive: bool) -> Option<(Value, bool)> {
    match current {
        None => Some((value.clone(), inclusive)),
        Some((bound, bound_inclusive)) => match value_cmp(&bound, value) {
            std::cmp::Ordering::Greater => Some((value.clone(), inclusive)),
            std::cmp::Ordering::Equal if bound_inclusive && !inclusive => {
                Some((value.clone(), inclusive))
            }
            _ => Some((bound, bound_inclusive)),
        },
    }
}

/// Intersect candidate id sets smallest-first, short-circuiting the moment an
/// empty one appears. `None` means nothing to intersect — no index applies.
pub(crate) fn intersect_sets(sets: Vec<HashSet<String>>) -> Option<HashSet<String>> {
    if sets.is_empty() {
        return None;
    }
    if sets.iter().any(|set| set.is_empty()) {
        return Some(HashSet::new());
    }
    let mut sets = sets;
    sets.sort_by_key(|set| set.len());
    let mut iter = sets.into_iter();
    let mut acc = iter.next().expect("sets is non-empty");
    for other in iter {
        if acc.len() > other.len() {
            // Always drive the intersection from the smaller side.
            let mut next = other;
            next.retain(|id| acc.contains(id));
            acc = next;
        } else {
            acc.retain(|id| other.contains(id));
        }
        if acc.is_empty() {
            break;
        }
    }
    Some(acc)
}

// ─── Path-based Mutation Helpers ─────────────────────────────────────

/// Returns true when the assignment landed (path resolved, value written).
/// False is a no-op: the path could not be resolved, nothing changed.
pub(crate) fn apply_path_set(doc: &mut Value, path: &str, value: Value) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() {
        return false;
    }
    walk_and_set(doc, &segments, 0, value)
}

fn walk_and_set(current: &mut Value, segments: &[&str], depth: usize, value: Value) -> bool {
    if depth >= segments.len() {
        return false;
    }
    let key = segments[depth];
    let is_last = depth + 1 == segments.len();

    if let Ok(idx) = key.parse::<usize>() {
        if let Some(arr) = current.as_array_mut() {
            if idx < arr.len() {
                if is_last {
                    arr[idx] = value;
                    return true;
                }
                return walk_and_set(&mut arr[idx], segments, depth + 1, value);
            }
        }
    } else if let Some(obj) = current.as_object_mut() {
        if is_last {
            // Assignment is applied even when the value is identical —
            // idempotent re-assignment asserts state, it is not a no-op.
            obj.insert(key.to_string(), value);
            return true;
        } else if obj.contains_key(key) {
            let next = obj.get_mut(key).unwrap();
            return walk_and_set(next, segments, depth + 1, value);
        }
    }
    false
}

/// Returns true when something was removed, false when the path did not
/// resolve (idempotent remove of a missing key is a no-op).
pub(crate) fn apply_path_remove(doc: &mut Value, path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() {
        return false;
    }
    walk_and_remove(doc, &segments, 0)
}

/// Push a value onto the array at a dot-separated path. Array-aware:
/// "threads.2.messages" addresses nested arrays. If the final key doesn't
/// exist (or isn't an array), an array containing the value is created there —
/// mirroring the top-level behavior.
/// Returns true when the push landed (existing array extended, or the
/// array created). False is a no-op: path did not resolve, or the target
/// exists but is not an array.
pub(crate) fn apply_path_push(doc: &mut Value, path: &str, value: Value) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    if segments.is_empty() {
        return false;
    }
    walk_and_push(doc, &segments, 0, value)
}

fn walk_and_push(current: &mut Value, segments: &[&str], depth: usize, value: Value) -> bool {
    if depth >= segments.len() {
        return false;
    }
    let key = segments[depth];
    let is_last = depth + 1 == segments.len();

    if let Ok(idx) = key.parse::<usize>() {
        if let Some(arr) = current.as_array_mut() {
            if idx < arr.len() {
                if is_last {
                    if let Some(inner) = arr[idx].as_array_mut() {
                        inner.push(value);
                        return true;
                    }
                    return false;
                }
                return walk_and_push(&mut arr[idx], segments, depth + 1, value);
            }
        }
        return false;
    }

    if let Some(obj) = current.as_object_mut() {
        if is_last {
            return match obj.get_mut(key) {
                Some(slot) => {
                    if let Some(inner) = slot.as_array_mut() {
                        inner.push(value);
                        return true;
                    }
                    false
                }
                None => {
                    obj.insert(key.to_string(), serde_json::json!([value]));
                    true
                }
            };
        }
        if obj.contains_key(key) {
            return walk_and_push(obj.get_mut(key).unwrap(), segments, depth + 1, value);
        }
    }
    false
}

fn walk_and_remove(current: &mut Value, segments: &[&str], depth: usize) -> bool {
    if depth >= segments.len() {
        return false;
    }
    let key = segments[depth];
    let is_last = depth + 1 == segments.len();

    if let Ok(idx) = key.parse::<usize>() {
        if let Some(arr) = current.as_array_mut() {
            if idx < arr.len() {
                if is_last {
                    arr.remove(idx);
                    return true;
                }
                return walk_and_remove(&mut arr[idx], segments, depth + 1);
            }
        }
    } else if let Some(obj) = current.as_object_mut() {
        if is_last {
            return obj.remove(key).is_some();
        } else if obj.contains_key(key) {
            let next = obj.get_mut(key).unwrap();
            return walk_and_remove(next, segments, depth + 1);
        }
    }
    false
}

// ─── Database ───────────────────────────────────────────────────────

/// Apply one journal entry to replayed state and return the change it
/// represents. `None` means the entry changed nothing — a patch whose target
/// document does not exist yet, an unknown op, or an unresolvable path.
fn replay_journal_entry(
    docs: &mut HashMap<String, Value>,
    deleted: &mut HashSet<String>,
    entry: &Value,
) -> Option<(String, ChangeKind)> {
    let id = entry.get("_id").and_then(|v| v.as_str())?.to_string();

    if entry.get("_deleted").is_some() {
        deleted.insert(id.clone());
        docs.remove(&id);
        return Some((id, ChangeKind::Delete));
    }

    if let Some(op) = entry.get("_op").and_then(|v| v.as_str()) {
        let applied = {
            let doc = docs.get_mut(&id)?;
            match op {
                // Accept "path" (new, dot-notation) or "field" (legacy, top-level)
                "array_push" => entry
                    .get("path")
                    .or_else(|| entry.get("field"))
                    .and_then(|v| v.as_str())
                    .zip(entry.get("value"))
                    .map_or(false, |(path, value)| {
                        apply_path_push(doc, path, value.clone())
                    }),
                "set" => entry
                    .get("path")
                    .and_then(|v| v.as_str())
                    .zip(entry.get("value"))
                    .map_or(false, |(path, value)| {
                        apply_path_set(doc, path, value.clone())
                    }),
                "remove" => entry
                    .get("path")
                    .and_then(|v| v.as_str())
                    .map_or(false, |path| apply_path_remove(doc, path)),
                _ => false,
            }
        };
        if !applied {
            return None;
        }
        let after = docs.get(&id)?.clone();
        return Some((id, ChangeKind::Upsert(after)));
    }

    deleted.remove(&id);
    docs.insert(id.clone(), entry.clone());
    Some((id, ChangeKind::Upsert(entry.clone())))
}

/// What happens to a document's files when the document is deleted,
/// declared per bucket in `meta.json` (#10).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum OnDocumentDelete {
    /// Refuse the delete while the document references files in this bucket.
    Restrict,
    /// Release the document's refs; orphaned files move to trash.
    /// This is the explicit form of the default behavior.
    Trash,
}

/// The bucket's keying strategy (#9). Absence of `kind` means `"hash"` —
/// existing databases activate nothing by upgrading the binary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum BucketKind {
    /// Content-keyed blobs: SHA-256 dedup, refcounted. The default.
    Hash,
    /// Item-keyed folders: engine-issued id, app streams bytes itself.
    Items,
}

/// Per-bucket policy parsed from `meta.json` (`buckets.<name>`, #9/#10).
#[derive(Clone, Debug, Default)]
pub(crate) struct BucketPolicy {
    pub(crate) on_document_delete: Option<OnDocumentDelete>,
    /// Per-bucket trash TTL, overriding the database-wide one at purge.
    pub(crate) ttl_seconds: Option<u64>,
    kind: Option<BucketKind>,
    /// Reserved-item TTL for `kind: "items"` buckets (default 30 min).
    reserved_ttl_seconds: Option<u64>,
}

/// Read `meta.json`'s bucket policy block. A missing file or block is not
/// an error — it means "no policy", which is today's behavior. A
/// *malformed* policy fails loudly: an admin config with a typo must not
/// silently degrade to no-policy. Unknown top-level keys (`schemas`,
/// future fields) are ignored.
fn load_bucket_policies(base_dir: &Path) -> Result<HashMap<String, BucketPolicy>> {
    let meta_path = base_dir.join("meta.json");
    if !meta_path.exists() {
        return Ok(HashMap::new());
    }
    let raw = fs::read_to_string(&meta_path)
        .map_err(Error::io_err(&meta_path, "read meta.json"))?;
    let meta: Value = serde_json::from_str(&raw)
        .map_err(|e| Error::corruption(&meta_path, format!("invalid JSON: {e}")))?;

    let mut policies = HashMap::new();
    let buckets = match meta.get("buckets") {
        None => return Ok(policies),
        Some(Value::Null) => return Ok(policies),
        Some(b) if b.is_object() => b.as_object().unwrap(),
        // Legacy CLI form: `ndb init`/`merge`/`recover` wrote name arrays
        // before bucket policies existed (#10). A string entry is a bucket
        // name with no policy; anything else in the array is malformed.
        Some(Value::Array(items)) => {
            for item in items {
                let name = item.as_str().ok_or_else(|| {
                    Error::corruption(
                        &meta_path,
                        format!("'buckets' array entries must be bucket-name strings, got {item}"),
                    )
                })?;
                policies.insert(name.to_string(), BucketPolicy::default());
            }
            return Ok(policies);
        }
        Some(_) => {
            return Err(Error::corruption(&meta_path, "'buckets' must be an object"))
        }
    };
    for (name, block) in buckets {
        let block = block.as_object().ok_or_else(|| {
            Error::corruption(&meta_path, format!("buckets.{name} must be an object"))
        })?;
        let on_document_delete = match block.get("onDocumentDelete") {
            None => None,
            Some(Value::String(s)) if s == "restrict" => Some(OnDocumentDelete::Restrict),
            Some(Value::String(s)) if s == "trash" => Some(OnDocumentDelete::Trash),
            Some(v) => {
                return Err(Error::corruption(
                    &meta_path,
                    format!("buckets.{name}.onDocumentDelete must be \"restrict\" or \"trash\", got {v}"),
                ))
            }
        };
        let ttl_seconds = match block.get("ttl_seconds") {
            None => None,
            Some(v) => Some(v.as_u64().ok_or_else(|| {
                Error::corruption(
                    &meta_path,
                    format!("buckets.{name}.ttl_seconds must be a non-negative integer, got {v}"),
                )
            })?),
        };
        let kind = match block.get("kind") {
            None => None,
            Some(Value::String(s)) if s == "hash" => Some(BucketKind::Hash),
            Some(Value::String(s)) if s == "items" => Some(BucketKind::Items),
            Some(v) => {
                return Err(Error::corruption(
                    &meta_path,
                    format!("buckets.{name}.kind must be \"hash\" or \"items\", got {v}"),
                ))
            }
        };
        let reserved_ttl_seconds = match block.get("reserved_ttl_seconds") {
            None => None,
            Some(v) => Some(v.as_u64().ok_or_else(|| {
                Error::corruption(
                    &meta_path,
                    format!("buckets.{name}.reserved_ttl_seconds must be a non-negative integer, got {v}"),
                )
            })?),
        };
        policies.insert(
            name.clone(),
            BucketPolicy {
                on_document_delete,
                ttl_seconds,
                kind,
                reserved_ttl_seconds,
            },
        );
    }
    Ok(policies)
}

/// An OS-enforced exclusive lock on one database.
///
/// Held for the whole `Database` lifetime; released by `Drop` or by the OS when
/// the process dies, so a crashed process never leaves a permanently locked
/// database. A second mount of the same journal fails fast instead of letting
/// two writers interleave into one file (C2).
struct DbLock {
    /// Closing this handle releases the lock. The file is deliberately never
    /// unlinked: deleting it after unlocking would let a third opener create a
    /// fresh inode and lock a file the second opener does not have.
    _file: fs::File,
}

impl DbLock {
    fn acquire(db_path: &Path) -> Result<Self> {
        let lock_path = Self::lock_path(db_path);
        if let Some(parent) = lock_path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(Error::io_err(parent, "create lock directory"))?;
            }
        }

        let file = Self::open_exclusive(&lock_path)
            .map_err(|e| Self::map_open_error(db_path, &lock_path, e))?;

        // Informational only — the handle, not the contents, is the lock.
        let _ = file.set_len(0);
        let mut file = file;
        let _ = writeln!(file, "pid={} at={}", std::process::id(), now_secs());

        Ok(DbLock { _file: file })
    }

    /// `<file>.lock` for a flat journal, `.lock` for a database folder's
    /// `data.jsonl` (the folder is the database, so the folder owns the lock).
    fn lock_path(db_path: &Path) -> PathBuf {
        let name = db_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| "data.jsonl".to_string());
        if name == "data.jsonl" {
            db_path.with_file_name(".lock")
        } else {
            db_path.with_file_name(format!("{name}.lock"))
        }
    }

    #[cfg(windows)]
    fn open_exclusive(lock_path: &Path) -> std::io::Result<fs::File> {
        use std::os::windows::fs::OpenOptionsExt;
        // FILE_SHARE_READ: readers may look, but nobody else may open for
        // write, delete or rename while this handle lives.
        const FILE_SHARE_READ: u32 = 0x0000_0001;
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .share_mode(FILE_SHARE_READ)
            .open(lock_path)
    }

    #[cfg(unix)]
    fn open_exclusive(lock_path: &Path) -> std::io::Result<fs::File> {
        use std::os::unix::io::AsRawFd;
        extern "C" {
            fn flock(fd: i32, operation: i32) -> i32;
        }
        const LOCK_EX: i32 = 2;
        const LOCK_NB: i32 = 4;
        let file = fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(lock_path)?;
        // SAFETY: `fd` is owned by `file` for the duration of the call.
        if unsafe { flock(file.as_raw_fd(), LOCK_EX | LOCK_NB) } != 0 {
            return Err(std::io::Error::last_os_error());
        }
        Ok(file)
    }

    #[cfg(not(any(windows, unix)))]
    fn open_exclusive(lock_path: &Path) -> std::io::Result<fs::File> {
        // No portable primitive without a crate: the file exists so operators
        // see one, but exclusivity is best-effort on this platform.
        report::warning(
            "database lock",
            format!("{}: no OS lock primitive on this platform", lock_path.display()),
        );
        fs::OpenOptions::new()
            .write(true)
            .create(true)
            .open(lock_path)
    }

    #[cfg(windows)]
    fn map_open_error(db_path: &Path, lock_path: &Path, e: std::io::Error) -> Error {
        // 32 = ERROR_SHARING_VIOLATION, 33 = ERROR_LOCK_VIOLATION: the other
        // holder opened the file with FILE_SHARE_READ only.
        match e.raw_os_error() {
            Some(32) | Some(33) => Error::DatabaseLocked {
                path: db_path.to_path_buf(),
            },
            _ if e.kind() == std::io::ErrorKind::PermissionDenied => Error::DatabaseLocked {
                path: db_path.to_path_buf(),
            },
            _ => Error::io_err(lock_path, "open database lock file")(e),
        }
    }

    #[cfg(not(windows))]
    fn map_open_error(db_path: &Path, lock_path: &Path, e: std::io::Error) -> Error {
        if e.kind() == std::io::ErrorKind::WouldBlock {
            Error::DatabaseLocked {
                path: db_path.to_path_buf(),
            }
        } else if let Some(code) = e.raw_os_error() {
            // EWOULDBLOCK/EAGAIN
            if code == 11 || code == 35 {
                return Error::DatabaseLocked {
                    path: db_path.to_path_buf(),
                };
            }
            Error::io_err(lock_path, "open database lock file")(e)
        } else {
            Error::io_err(lock_path, "open database lock file")(e)
        }
    }
}

impl Drop for DbLock {
    fn drop(&mut self) {
        // Nothing to do: closing `_file` is the release. The file stays on
        // disk on purpose (see the struct comment).
    }
}

pub(crate) fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

/// The database's OS writer lock, held without loading the database.
///
/// Maintenance that rewrites the journal in place (`ndb repair`) must take it:
/// truncating a journal under a live writer would destroy exactly the data the
/// lock exists to protect. Dropping the guard releases the lock.
pub struct MaintenanceLock {
    _lock: DbLock,
}

impl MaintenanceLock {
    /// Take the lock for `journal_path` (`…/data.jsonl` or a flat `.jsonl`).
    pub fn acquire(journal_path: impl AsRef<Path>) -> Result<Self> {
        Ok(MaintenanceLock {
            _lock: DbLock::acquire(journal_path.as_ref())?,
        })
    }
}

/// One committed change as kept in RAM: the document is *shared*, not copied —
/// the store and the change log point at the same `Arc<Value>`, so a write
/// materializes one document, not four.
#[derive(Debug, Clone)]
pub(crate) struct LoggedChange {
    pub(crate) seq: u64,
    pub(crate) id: String,
    pub(crate) kind: LoggedKind,
}

#[derive(Debug, Clone)]
pub(crate) enum LoggedKind {
    Upsert(std::sync::Arc<Value>),
    Delete,
}

fn logged_kind(kind: ChangeKind) -> LoggedKind {
    match kind {
        ChangeKind::Upsert(doc) => LoggedKind::Upsert(std::sync::Arc::new(doc)),
        ChangeKind::Delete => LoggedKind::Delete,
    }
}

/// A journal line: the entry's own fields plus the authoritative `_seq`,
/// serialized straight from the borrowed document — stamping the sequence
/// costs no copy. A caller-supplied `_seq` is dropped (the engine owns it).
struct JournalRecord<'a> {
    object: &'a serde_json::Map<String, Value>,
    seq: u64,
}

impl serde::Serialize for JournalRecord<'_> {
    fn serialize<S: serde::Serializer>(
        &self,
        serializer: S,
    ) -> std::result::Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.object.len() + 1))?;
        for (key, value) in self.object {
            if key == storage::SEQ_FIELD {
                continue;
            }
            map.serialize_entry(key, value)?;
        }
        map.serialize_entry(storage::SEQ_FIELD, &self.seq)?;
        map.end()
    }
}

// ─── Shared Query Planner ──────────────────────────────────────────

/// Index access the planner needs. Implemented by the v1 `Database` and by the
/// v2 engine, so both run the *same* typed planner — never a second one.
pub(crate) trait IndexAccess {
    fn lookup(&self, field: &str, value: &Value) -> Option<HashSet<String>>;
    fn range_scan(
        &self,
        field: &str,
        min: KeyBound<'_>,
        max: KeyBound<'_>,
    ) -> Option<HashSet<String>>;
    fn in_values(&self, field: &str, operand: &Value) -> Option<HashSet<String>>;
}

/// Candidate ids for `ast`, or `None` when no conjunct is indexable.
///
/// Every returned set is a **superset** of what `query_matches` would accept,
/// which is the only property the caller needs: candidates are re-evaluated
/// with `query_matches` before being returned. Combinators are mirrored
/// exactly — `query_matches` returns early on `$and`/`$or`/`$not`, so sibling
/// field keys of a combinator are not constraints and are not planned.
pub(crate) fn plan_candidates<A: IndexAccess + ?Sized>(
    ast: &Value,
    access: &A,
) -> Option<HashSet<String>> {
    match ast {
        Value::Object(map) => {
            if let Some(and_expr) = map.get("$and") {
                let children = and_expr.as_array()?;
                return intersect_plans(children.iter(), access);
            }
            if map.contains_key("$or") || map.contains_key("$not") {
                return None;
            }
            let mut sets = Vec::new();
            for (field, condition) in map {
                if let Some(set) = condition_candidates(field, condition, access) {
                    sets.push(set);
                }
            }
            intersect_sets(sets)
        }
        Value::Array(conditions) => intersect_plans(conditions.iter(), access),
        _ => None,
    }
}

fn intersect_plans<'a, I, A>(children: I, access: &A) -> Option<HashSet<String>>
where
    I: Iterator<Item = &'a Value>,
    A: IndexAccess + ?Sized,
{
    let mut sets = Vec::new();
    for child in children {
        if let Some(set) = plan_candidates(child, access) {
            sets.push(set);
        }
    }
    intersect_sets(sets)
}

/// Index candidates for one field condition.
///
/// Every operator in the object is a necessary condition, so the per-operator
/// candidate sets are intersected (not unioned) — a tighter, still-correct
/// superset. Range bounds on the same field are merged into a single BTree
/// scan first: intersecting interval scans one by one would materialize the
/// widest set just to throw it away.
fn condition_candidates<A: IndexAccess + ?Sized>(
    field: &str,
    condition: &Value,
    access: &A,
) -> Option<HashSet<String>> {
    let Value::Object(ops) = condition else {
        // Implicit `$eq`.
        return access.lookup(field, condition);
    };
    let mut sets = Vec::new();
    // (bound value, inclusive) — kept owned so a chosen bound outlives the
    // loop that found it.
    let mut lower: Option<(Value, bool)> = None;
    let mut upper: Option<(Value, bool)> = None;

    for (op, operand) in ops {
        match op.as_str() {
            "$eq" => {
                if let Some(set) = access.lookup(field, operand) {
                    sets.push(set);
                }
            }
            "$in" => {
                if let Some(set) = access.in_values(field, operand) {
                    sets.push(set);
                }
            }
            "$gt" => lower = stronger_lower(lower, operand, false),
            "$gte" => lower = stronger_lower(lower, operand, true),
            "$lt" => upper = stronger_upper(upper, operand, false),
            "$lte" => upper = stronger_upper(upper, operand, true),
            // $ne / $nin / $exists do not narrow a candidate set.
            _ => {}
        }
    }

    if lower.is_some() || upper.is_some() {
        let min = match &lower {
            None => KeyBound::Unbounded,
            Some((v, true)) => KeyBound::Included(v),
            Some((v, false)) => KeyBound::Excluded(v),
        };
        let max = match &upper {
            None => KeyBound::Unbounded,
            Some((v, true)) => KeyBound::Included(v),
            Some((v, false)) => KeyBound::Excluded(v),
        };
        if let Some(set) = access.range_scan(field, min, max) {
            sets.push(set);
        }
    }

    intersect_sets(sets)
}

impl IndexAccess for Database {
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

/// The main nDB database.
///
/// In-memory document store backed by JSON Lines persistence.
/// Single-writer, multi-reader concurrency model.
pub struct Database {
    /// Path to the JSONL data file.
    path: PathBuf,
    /// Base directory for the database (contains active/, _trash/, _files/).
    base_dir: PathBuf,
    /// In-memory document store: _id → document. Documents are shared with
    /// the change log (`Arc`), so nothing here deep-clones on a write.
    docs: RwLock<HashMap<String, std::sync::Arc<Value>>>,
    /// Set of deleted document IDs (tombstones).
    deleted: RwLock<HashSet<String>>,
    /// In-memory file reference counter: nURI string → count.
    file_refs: RwLock<HashMap<String, usize>>,
    /// Hash indexes (opt-in, O(1) equality / `$in` lookups).
    hash_indexes: RwLock<HashMap<String, HashIndex>>,
    /// BTree indexes (opt-in, O(log n) equality + exact range scans). A field
    /// may carry both kinds; the planner prefers whichever fits the operator.
    btree_indexes: RwLock<HashMap<String, BTreeIndex>>,    /// Full-text indexes (opt-in): field → inverted index.
    text_indexes: RwLock<HashMap<String, search::TextIndex>>,
    /// Single-writer mutex. Arc-shared with the TTL thread so its
    /// reserved-item sweep serializes against writers exactly like the
    /// public item API does (G3).
    writer: std::sync::Arc<Mutex<()>>,
    /// Serializes the document-trash file: delete()'s restorable-record
    /// append against the purgers' read-all→rewrite (B3). Always taken
    /// AFTER `writer` when both are held; the purge paths never take
    /// `writer`, so no lock inversion is possible.
    trash_lock: std::sync::Arc<Mutex<()>>,
    /// Persistence mode.
    persistence: Persistence,
    /// Trash mode.
    trash_mode: TrashMode,
    /// Auto-purge TTL duration.
    trash_ttl: Option<Duration>,
    /// Interval for TTL background check.
    trash_purge_interval: Option<Duration>,
    /// Channel sender to wake/terminate the TTL thread on Drop.
    ttl_tx: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// Background thread handle for TTL purging.
    ttl_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// Append-only file handle (held open for writes).
    file_handle: Mutex<Option<fs::File>>,
    /// Per-bucket policies from `meta.json`, loaded at open (#10).
    /// Empty means "no policy" — the pre-meta.json behavior.
    bucket_policies: HashMap<String, BucketPolicy>,
    /// Item buckets declared `kind: "items"` in `meta.json` (#9).
    item_buckets: HashMap<String, std::sync::Arc<ItemBucket>>,
    /// Journal identity and replay window. Written only under `writer`.
    lineage: RwLock<SourceLineage>,
    /// Committed change events, oldest first. Cleared by compaction, which is
    /// what makes everything before the new `min_available_seq` unreplayable.
    change_log: RwLock<Vec<LoggedChange>>,
    /// Commit seq of each live document — the per-document optimistic
    /// concurrency token. Kept per document (not from the global `last_seq`) so
    /// concurrent edits to other documents never produce a false conflict.
    doc_seq: RwLock<HashMap<String, u64>>,
    /// OS-exclusive writer lock; `None` for an in-memory database.
    _lock: Option<DbLock>,
    /// Set when an append has not been fsynced yet (Scheduled/Lazy).
    dirty: std::sync::Arc<AtomicBool>,
    /// Cancellation channel for the Scheduled flush thread.
    flush_tx: Mutex<Option<std::sync::mpsc::Sender<()>>>,
    /// Scheduled flush thread handle.
    flush_thread: Mutex<Option<std::thread::JoinHandle<()>>>,
    /// The v2 backend, when this handle was opened on a binary/mmap database.
    /// All document traffic is routed here; the v1 fields stay empty.
    v2: Option<Arc<v2_engine::V2Engine>>,
}

impl Database {
    /// Open or create a database at the given path.
    ///
    /// Format is detected unless [`OpenOptions::format`] forces one: a folder
    /// holding `manifest.json` opens on the v2 binary/mmap backend, a folder
    /// holding `data.jsonl` (or an empty one) opens on the v1 JSONL backend.
    ///
    /// If the v1 file exists, loads all documents into memory and rebuilds the
    /// change feed from the journal: lines carrying `_seq` become events,
    /// base-state lines of a compacted file do not, and a pre-lineage journal
    /// replays every line as a commit (its epoch is derived from `_meta.created`
    /// so the same uncompacted file keeps the same identity across boots).
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_with_options(path, OpenOptions::default())
    }

    /// Open a v2 binary/mmap database folder.
    pub fn open_v2(path: impl AsRef<Path>) -> Result<Self> {
        Self::open_v2_with_options(
            path,
            OpenOptions {
                format: StorageFormat::V2,
                persistence: Persistence::default(),
            },
        )
    }

    /// Open a v2 database with explicit options. `format` is forced to
    /// [`StorageFormat::V2`]; every other field is honoured.
    pub fn open_v2_with_options(
        path: impl AsRef<Path>,
        mut options: OpenOptions,
    ) -> Result<Self> {
        options.format = StorageFormat::V2;
        Self::open_with_options(path, options)
    }

    /// Open a database with explicit options (format detection and
    /// persistence selection).
    pub fn open_with_options(path: impl AsRef<Path>, options: OpenOptions) -> Result<Self> {
        let raw = path.as_ref().to_path_buf();
        // A directory names a database folder; anything else names the journal
        // (whose parent is the folder). A v2 folder that does not exist yet is
        // still a folder — a v2 database is never a bare `.jsonl` file.
        let treat_as_dir = raw.is_dir()
            || (options.format == StorageFormat::V2
                && !raw.exists()
                && raw
                    .extension()
                    .map_or(true, |e| !e.to_string_lossy().eq_ignore_ascii_case("jsonl")));
        let (journal, base_dir) = if treat_as_dir {
            (raw.join("data.jsonl"), raw)
        } else {
            let base = raw
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf();
            (raw, base)
        };

        // The lock comes first: two processes must never both get as far as
        // creating/appending to the same database.
        if !base_dir.as_os_str().is_empty() {
            fs::create_dir_all(&base_dir)
                .map_err(Error::io_err(&base_dir, "create database directory"))?;
        }
        let lock = DbLock::acquire(&journal)?;

        let manifest = base_dir.join(v2_format::MANIFEST_FILE);
        let format = match options.format {
            StorageFormat::V1 => {
                if manifest.exists() {
                    return Err(Error::corruption(
                        &manifest,
                        "a v2 manifest exists but StorageFormat::V1 was requested",
                    ));
                }
                StorageFormat::V1
            }
            StorageFormat::V2 => StorageFormat::V2,
            StorageFormat::Auto if manifest.exists() => StorageFormat::V2,
            StorageFormat::Auto => StorageFormat::V1,
        };

        match format {
            StorageFormat::V2 => Self::open_v2_locked(journal, base_dir, lock, options.persistence),
            _ => Self::open_v1_locked(journal, base_dir, lock, options.persistence),
        }
    }

    /// Open a v2 database with the OS writer lock already held.
    fn open_v2_locked(
        journal: PathBuf,
        base_dir: PathBuf,
        lock: DbLock,
        persistence: Persistence,
    ) -> Result<Self> {
        let bucket_policies = load_bucket_policies(&base_dir)?;
        let trash_lock = Arc::new(Mutex::new(()));
        let file_refs = Arc::new(RwLock::new(HashMap::new()));

        let engine = Arc::new(v2_engine::V2Engine::open(
            &base_dir,
            bucket_policies.clone(),
            Arc::clone(&trash_lock),
            Arc::clone(&file_refs),
        )?);

        let mut item_buckets = HashMap::new();
        for (name, policy) in &bucket_policies {
            if policy.kind == Some(BucketKind::Items) {
                let ttl = Duration::from_secs(
                    policy
                        .reserved_ttl_seconds
                        .unwrap_or(item::DEFAULT_RESERVED_TTL_SECS),
                );
                item_buckets.insert(
                    name.clone(),
                    Arc::new(ItemBucket::open(name, &base_dir, ttl)?),
                );
            }
        }

        let mut db = Database {
            path: journal,
            base_dir,
            docs: RwLock::new(HashMap::new()),
            deleted: RwLock::new(HashSet::new()),
            file_refs: RwLock::new((*file_refs.read()).clone()),
            hash_indexes: RwLock::new(HashMap::new()),
            btree_indexes: RwLock::new(HashMap::new()),
            text_indexes: RwLock::new(HashMap::new()),
            writer: Arc::new(Mutex::new(())),
            trash_lock,
            persistence,
            trash_mode: TrashMode::Manual,
            trash_ttl: None,
            trash_purge_interval: None,
            ttl_tx: Mutex::new(None),
            ttl_thread: Mutex::new(None),
            file_handle: Mutex::new(None),
            bucket_policies,
            item_buckets,
            lineage: RwLock::new(engine.lineage()),
            change_log: RwLock::new(Vec::new()),
            doc_seq: RwLock::new(HashMap::new()),
            _lock: Some(lock),
            dirty: Arc::new(AtomicBool::new(false)),
            flush_tx: Mutex::new(None),
            flush_thread: Mutex::new(None),
            v2: Some(engine),
        };
        db.apply_persistence(persistence);
        db.start_flush_thread();
        Ok(db)
    }

    /// Open a v1 JSONL database with the OS writer lock already held.
    fn open_v1_locked(
        path: PathBuf,
        base_dir: PathBuf,
        lock: DbLock,
        persistence: Persistence,
    ) -> Result<Self> {
        // Ensure file exists
        if !path.exists() {
            storage::init_file(&path, &SourceLineage::fresh())?;
        }

        let (meta, raw_docs) = storage::read_all_with_meta(&path)?;

        let legacy = meta
            .as_ref()
            .map_or(true, |m| m.source_epoch_uuid.is_none());
        let source_epoch_uuid = match meta.as_ref().and_then(|m| m.source_epoch_uuid.clone()) {
            Some(uuid) => uuid,
            None => storage::legacy_epoch_uuid(meta.as_ref().map_or("", |m| m.created.as_str())),
        };
        let mut last_seq = meta.as_ref().map_or(0, |m| m.last_seq);
        let min_available_seq = meta.as_ref().map_or(1, |m| m.min_available_seq);

        // Build in-memory state: last write wins
        let mut docs: HashMap<String, Value> = HashMap::new();
        let mut deleted = HashSet::new();
        let mut change_log: Vec<LoggedChange> = Vec::new();
        let mut doc_seq: HashMap<String, u64> = HashMap::new();

        for mut entry in raw_docs {
            let seq = entry
                .as_object_mut()
                .and_then(|obj| obj.remove(storage::SEQ_FIELD))
                .and_then(|v| v.as_u64());
            let change = replay_journal_entry(&mut docs, &mut deleted, &entry);
            match seq {
                Some(seq) => {
                    last_seq = last_seq.max(seq);
                    if let Some((id, kind)) = change {
                        match kind {
                            ChangeKind::Upsert(_) => {
                                doc_seq.insert(id.clone(), seq);
                            }
                            ChangeKind::Delete => {
                                doc_seq.remove(&id);
                            }
                        }
                        change_log.push(LoggedChange {
                            seq,
                            id,
                            kind: logged_kind(kind),
                        });
                    }
                }
                None if legacy => {
                    if let Some((id, kind)) = change {
                        last_seq += 1;
                        match kind {
                            ChangeKind::Upsert(_) => {
                                doc_seq.insert(id.clone(), last_seq);
                            }
                            ChangeKind::Delete => {
                                doc_seq.remove(&id);
                            }
                        }
                        change_log.push(LoggedChange {
                            seq: last_seq,
                            id,
                            kind: logged_kind(kind),
                        });
                    }
                }
                None => {}
            }
        }

        // Initialize file reference counter
        let mut file_refs: HashMap<String, usize> = HashMap::new();
        for doc in docs.values() {
            let mut refs = HashSet::new();
            Self::extract_file_refs(doc, &mut refs);
            for r in refs {
                *file_refs.entry(r).or_insert(0) += 1;
            }
        }

        // Bucket policies from meta.json (#10) — loaded before the struct
        // literal moves base_dir.
        let bucket_policies = load_bucket_policies(&base_dir)?;

        // Item buckets (#9): one ItemBucket per bucket declared
        // `kind: "items"`, journal replayed at open. Absence of the
        // declaration means nothing activates.
        let mut item_buckets = HashMap::new();
        for (name, policy) in &bucket_policies {
            if policy.kind == Some(BucketKind::Items) {
                let ttl = Duration::from_secs(
                    policy
                        .reserved_ttl_seconds
                        .unwrap_or(item::DEFAULT_RESERVED_TTL_SECS),
                );
                item_buckets.insert(
                    name.clone(),
                    std::sync::Arc::new(ItemBucket::open(name, &base_dir, ttl)?),
                );
            }
        }

        let mut db = Database {
            path,
            base_dir,
            docs: RwLock::new(
                docs.into_iter()
                    .map(|(id, doc)| (id, std::sync::Arc::new(doc)))
                    .collect(),
            ),
            deleted: RwLock::new(deleted),
            file_refs: RwLock::new(file_refs),
            hash_indexes: RwLock::new(HashMap::new()),
            btree_indexes: RwLock::new(HashMap::new()),
            text_indexes: RwLock::new(HashMap::new()),
            writer: std::sync::Arc::new(Mutex::new(())),
            trash_lock: std::sync::Arc::new(Mutex::new(())),
            persistence: Persistence::Lazy,
            trash_mode: TrashMode::Manual,
            trash_ttl: None,
            trash_purge_interval: None,
            ttl_tx: Mutex::new(None),
            ttl_thread: Mutex::new(None),
            file_handle: Mutex::new(None),
            bucket_policies,
            item_buckets,
            lineage: RwLock::new(SourceLineage {
                source_epoch_uuid,
                last_seq,
                min_available_seq,
            }),
            change_log: RwLock::new(change_log),
            doc_seq: RwLock::new(doc_seq),
            _lock: Some(lock),
            dirty: std::sync::Arc::new(AtomicBool::new(false)),
            flush_tx: Mutex::new(None),
            flush_thread: Mutex::new(None),
            v2: None,
        };
        db.apply_persistence(persistence);
        db.start_flush_thread();
        Ok(db)
    }

    /// Open a purely in-memory database (no disk file).
    ///
    /// Carries a full lineage like an on-disk one: every mutation bumps `seq`
    /// and is recorded, so `lineage`/`snapshot_at`/`changes_since` behave
    /// identically.
    pub fn open_in_memory() -> Result<Self> {
        Ok(Database {
            path: PathBuf::new(),
            base_dir: PathBuf::new(),
            docs: RwLock::new(HashMap::new()),
            deleted: RwLock::new(HashSet::new()),
            file_refs: RwLock::new(HashMap::new()),
            hash_indexes: RwLock::new(HashMap::new()),
            btree_indexes: RwLock::new(HashMap::new()),
            text_indexes: RwLock::new(HashMap::new()),
            writer: std::sync::Arc::new(Mutex::new(())),
            trash_lock: std::sync::Arc::new(Mutex::new(())),
            persistence: Persistence::Lazy,
            trash_mode: TrashMode::Manual,
            trash_ttl: None,
            trash_purge_interval: None,
            ttl_tx: Mutex::new(None),
            ttl_thread: Mutex::new(None),
            file_handle: Mutex::new(None),
            bucket_policies: HashMap::new(),
            item_buckets: HashMap::new(),
            lineage: RwLock::new(SourceLineage::fresh()),
            change_log: RwLock::new(Vec::new()),
            doc_seq: RwLock::new(HashMap::new()),
            _lock: None,
            dirty: std::sync::Arc::new(AtomicBool::new(false)),
            flush_tx: Mutex::new(None),
            flush_thread: Mutex::new(None),
            v2: None,
        })
    }

    /// Set persistence mode. Returns self for chaining.
    ///
    /// `Scheduled(d)` starts a real background flusher: every `d` it fsyncs the
    /// journal (v1) or the WAL (v2) if anything was written since the last
    /// flush (H2). It is not a `Lazy` alias.
    pub fn with_persistence(mut self, mode: Persistence) -> Self {
        self.persistence = mode;
        self.apply_persistence(mode);
        self.start_flush_thread();
        self
    }

    /// Push the persistence mode into the v2 backend (no-op for v1).
    fn apply_persistence(&mut self, mode: Persistence) {
        self.persistence = mode;
        if let Some(v2) = &self.v2 {
            v2.set_persistence(mode);
        }
    }

    /// Set trash mode. Returns self for chaining.
    pub fn with_trash_mode(mut self, mode: TrashMode) -> Self {
        self.trash_mode = mode;
        if let Some(v2) = &self.v2 {
            v2.set_trash_mode(mode);
        }
        self
    }

    /// Set auto-trash TTL and the background interval to purge.
    pub fn with_trash_ttl(mut self, ttl: Duration, interval: Duration) -> Self {
        self.trash_ttl = Some(ttl);
        self.trash_purge_interval = Some(interval);
        self.start_ttl_thread();
        self
    }

    /// Start the real background flusher for `Persistence::Scheduled`.
    fn start_flush_thread(&mut self) {
        // A previous flusher (from an earlier `with_persistence`) must not be
        // left running: two timers would race the same WAL.
        if let Some(tx) = self.flush_tx.lock().take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.flush_thread.lock().take() {
            if handle.join().is_err() {
                report::detached("restart flush thread", "background thread panicked");
            }
        }

        let interval = match self.persistence {
            Persistence::Scheduled(d) => d,
            _ => return,
        };
        if self.is_in_memory() {
            return;
        }
        let interval = if interval.is_zero() {
            report::warning(
                "scheduled persistence",
                "interval must be greater than zero; using 100ms",
            );
            Duration::from_millis(100)
        } else {
            interval
        };

        let (tx, rx) = std::sync::mpsc::channel();
        *self.flush_tx.lock() = Some(tx);

        if let Some(engine) = &self.v2 {
            let engine = Arc::clone(engine);
            let handle = std::thread::spawn(move || loop {
                match rx.recv_timeout(interval) {
                    Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        if let Err(e) = engine.flush() {
                            report::detached("scheduled flush: fsync v2 WAL", e);
                        }
                    }
                }
            });
            *self.flush_thread.lock() = Some(handle);
            return;
        }

        let dirty = Arc::clone(&self.dirty);
        let path = self.path.clone();
        let handle = std::thread::spawn(move || loop {
            match rx.recv_timeout(interval) {
                Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                    if dirty.swap(false, Ordering::SeqCst) {
                        if let Err(e) = fsync_journal(&path) {
                            dirty.store(true, Ordering::SeqCst);
                            report::detached("scheduled flush: fsync journal", e);
                        }
                    }
                }
            }
        });
        *self.flush_thread.lock() = Some(handle);
    }

    /// Internal helper to start the TTL background thread using a cancellation channel.
    fn start_ttl_thread(&mut self) {        if self.is_in_memory() {
            return;
        }
        
        let interval = self.trash_purge_interval.unwrap();
        let base_dir = self.base_dir.clone();
        let trash_file = self.trash_doc_path();
        let mode = self.trash_mode;
        let ttl_dur = self.trash_ttl.unwrap();
        let bucket_policies = self.bucket_policies.clone();
        let persistence = self.persistence;
        let trash_lock = std::sync::Arc::clone(&self.trash_lock);
        let writer = std::sync::Arc::clone(&self.writer);
        let item_buckets: Vec<std::sync::Arc<ItemBucket>> =
            self.item_buckets.values().cloned().collect();

        let (tx, rx) = std::sync::mpsc::channel();
        *self.ttl_tx.lock() = Some(tx);

        let thread_handle = std::thread::spawn(move || {
            loop {
                match rx.recv_timeout(interval) {
                    Ok(_) => break, // Cancellation signal received via tx.send(())
                    Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {
                        // Time to purge. There is no caller on this path, so a
                        // failing sweep would otherwise retry in silence every
                        // interval for the life of the process.
                        if let Err(e) = Self::purge_trash_static(
                            &base_dir,
                            &trash_file,
                            mode,
                            Some(ttl_dur),
                            &bucket_policies,
                            &trash_lock,
                        ) {
                            report::detached("ttl sweep: purge expired trash", e);
                        }
                        // Reserved items past their TTL are swept on the
                        // same cadence (#9) — under the writer lock, exactly
                        // like the public sweep_reserved_items: without it,
                        // selection here is not atomic with a concurrent
                        // commit_item acting on the same reserved item (G3).
                        // G3: under the writer lock, exactly like the public
                        // sweep_reserved_items — without it, selection here
                        // is not atomic with a concurrent commit_item
                        // acting on the same reserved item.
                        {
                            let _w = writer.lock();
                            for bucket in &item_buckets {
                                if let Err(e) = bucket.sweep_reserved(persistence) {
                                    report::detached("ttl sweep: sweep reserved items", e);
                                }
                            }
                        }
                    }
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break, // DB dropped
                }
            }
        });

        *self.ttl_thread.lock() = Some(thread_handle);
    }

    /// Static version of purge_trash that doesn't need `&self`, used by the background thread.
    fn purge_trash_static(
        base_dir: &Path,
        trash_file: &Path,
        trash_mode: TrashMode,
        trash_ttl: Option<Duration>,
        bucket_policies: &HashMap<String, BucketPolicy>,
        trash_lock: &Mutex<()>,
    ) -> Result<usize> {
        let ttl = match (trash_mode, trash_ttl) {
            (TrashMode::TTL(t), _) => t,
            (_, Some(t)) => t,
            _ => Duration::ZERO,
        };

        if ttl == Duration::ZERO {
            return Ok(0);
        }

        // Same B3 contract as purge_trash(): serialize the doc-trash
        // read-all→rewrite against delete()'s record append.
        let _trash_guard = trash_lock.lock();

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cutoff = now.saturating_sub(ttl.as_secs());

        let mut purged_count = 0;
        if trash_file.exists() {
            let all_trash = storage::read_trash(trash_file)?;
            let mut keep: Vec<&Value> = Vec::new();
            for doc in &all_trash {
                if let Some(deleted) = doc.get("_deleted").and_then(|v| v.as_u64()) {
                    if deleted > cutoff {
                        keep.push(doc);
                    } else {
                        purged_count += 1;
                    }
                } else {
                    keep.push(doc);
                }
            }
            if purged_count > 0 {
                storage::rewrite_atomic(trash_file, &keep)?;
            }
        }

        let trash_files_dir = base_dir.join("_trash").join("files");
        if trash_files_dir.exists() {
            // Detached path — there is no caller to fail to. But a directory or
            // entry we cannot read means that slice of the file trash was never
            // swept, which must not pass unremarked either.
            let entries = match std::fs::read_dir(&trash_files_dir) {
                Ok(entries) => entries,
                Err(e) => {
                    report::detached(
                        "ttl sweep: list bucket trash directory",
                        format!("{}: {e}", trash_files_dir.display()),
                    );
                    return Ok(purged_count);
                }
            };
            for entry in entries {
                let entry = match entry {
                    Ok(entry) => entry,
                    Err(e) => {
                        report::detached("ttl sweep: read bucket trash entry", e);
                        continue;
                    }
                };
                let file_type = match entry.file_type() {
                    Ok(file_type) => file_type,
                    Err(e) => {
                        report::detached("ttl sweep: stat bucket trash entry", e);
                        continue;
                    }
                };
                if file_type.is_dir() {
                    let name = entry.file_name().to_string_lossy().to_string();
                    let bucket = FileBucket::new(&name, base_dir);
                    let bucket_ttl = bucket_policies
                        .get(&name)
                        .and_then(|p| p.ttl_seconds)
                        .map(Duration::from_secs)
                        .unwrap_or(ttl);
                    if let Err(e) = bucket.purge_trash_ttl(bucket_ttl) {
                        report::detached("ttl sweep: purge bucket trash", e);
                    }
                }
            }
        }

        Ok(purged_count)
    }

    /// Check if this is an in-memory only database.
    fn is_in_memory(&self) -> bool {
        self.path.as_os_str().is_empty()
    }

    /// Get or create the append file handle.
    /// Returns a parking_lot MutexGuard.
    fn get_file_handle(&self) -> Result<parking_lot::MutexGuard<'_, Option<fs::File>>> {
        let mut handle = self.file_handle.lock();
        if handle.is_none() && !self.is_in_memory() {
            let file = storage::open_for_append(&self.path)?;
            *handle = Some(file);
        }
        Ok(handle)
    }

    /// Journal one committed mutation: append the line carrying `_seq`, then
    /// advance the lineage and record the change event.
    ///
    /// Caller holds `writer`, which is what makes `last_seq + 1` unique.
    /// `journal` is the entry written to disk (full document, patch or
    /// tombstone) and is never mutated — `_seq` is spliced into the serialized
    /// line, so no document copy is made to stamp it. `event` is the
    /// document's post-mutation state for an upsert (shared with the store and
    /// the change log), `None` for a delete.
    ///
    /// A failed append moves nothing: `seq`, the lineage and the change log
    /// stay exactly where they were, so the caller can retry whole.
    fn commit_mutation(
        &self,
        id: &str,
        journal: &Value,
        event: Option<std::sync::Arc<Value>>,
    ) -> Result<u64> {
        let seq = self.lineage.read().last_seq + 1;
        let object = journal
            .as_object()
            .ok_or_else(|| Error::invalid_arg("journal entry must be a JSON object"))?;
        let line = serde_json::to_string(&JournalRecord { object, seq })?;

        if !self.is_in_memory() {
            let mut handle = self.get_file_handle()?;
            if let Some(ref mut file) = *handle {
                match self.persistence {
                    Persistence::Immediate => {
                        storage::append_line_sync(file, &self.path, &line)?;
                    }
                    _ => {
                        storage::append_line(file, &self.path, &line)?;
                        self.dirty.store(true, Ordering::SeqCst);
                    }
                }
            }
        }

        self.lineage.write().last_seq = seq;
        {
            let mut doc_seq = self.doc_seq.write();
            if event.is_some() {
                doc_seq.insert(id.to_string(), seq);
            } else {
                doc_seq.remove(id);
            }
        }
        self.change_log.write().push(LoggedChange {
            seq,
            id: id.to_string(),
            kind: match event {
                // Engine-owned: a caller-supplied `_seq` never reaches the
                // store or the event — `JournalRecord` skips it on the way out.
                Some(doc) => LoggedKind::Upsert(doc),
                None => LoggedKind::Delete,
            },
        });
        Ok(seq)
    }

    // ─── Layer 1: Core Operations ──────────────────────────────────

    /// Insert a document. Generates a NanoID `_id` and returns it.
    /// O(1) operation: one collision check + HashMap insert + file append.
    pub fn insert(&self, mut doc: Value) -> Result<String> {
        if let Some(v2) = &self.v2 {
            return v2.insert(doc, None);
        }
        let _guard = self.writer.lock();

        // O(1) per attempt: no key enumeration. The check covers live *and*
        // tombstoned ids, so a re-insert can never resurrect a deleted id.
        let id = generate_checked(|candidate| {
            self.docs.read().contains_key(candidate)
                || self.deleted.read().contains(candidate)
        });
        // Caller-supplied shape: never panic on it. A JSON scalar (a
        // stringified string, null, a number, an array) is valid JSON but not
        // a document, and the caller must hear that rather than lose a process.
        let object = doc
            .as_object_mut()
            .ok_or_else(|| Error::invalid_arg("document must be a JSON object"))?;
        object.insert("_id".to_string(), Value::String(id.clone()));
        object.remove(storage::SEQ_FIELD);

        // One document instance, shared by the store, the change log and the
        // journal line — the write path copies nothing.
        let stored = std::sync::Arc::new(doc);
        self.commit_mutation(&id, &stored, Some(std::sync::Arc::clone(&stored)))?;

        self.index_insert_doc(&id, &stored);
        self.increment_file_refs(&stored);

        // Update in-memory store
        self.deleted.write().remove(&id);
        self.docs.write().insert(id.clone(), stored);

        // Full-text indexes (after the doc is in the store)
        self.retext_doc(&id, &Value::Null);

        Ok(id)
    }

    /// Insert a document with a prefixed ID.
    pub fn insert_with_prefix(&self, prefix: &str, mut doc: Value) -> Result<String> {
        if let Some(v2) = &self.v2 {
            return v2.insert(doc, Some(prefix.to_string()));
        }
        let _guard = self.writer.lock();

        let id = generate_checked_with_prefix(prefix, |candidate| {
            self.docs.read().contains_key(candidate)
                || self.deleted.read().contains(candidate)
        });
        // See `insert`: a non-object is caller error, surfaced not aborted on.
        let object = doc
            .as_object_mut()
            .ok_or_else(|| Error::invalid_arg("document must be a JSON object"))?;
        object.insert("_id".to_string(), Value::String(id.clone()));
        object.remove(storage::SEQ_FIELD);

        let stored = std::sync::Arc::new(doc);
        self.commit_mutation(&id, &stored, Some(std::sync::Arc::clone(&stored)))?;

        self.index_insert_doc(&id, &stored);
        self.increment_file_refs(&stored);

        self.deleted.write().remove(&id);
        self.docs.write().insert(id.clone(), stored);

        // Full-text indexes (after the doc is in the store)
        self.retext_doc(&id, &Value::Null);

        Ok(id)
    }

    /// Get a document by ID. O(1) HashMap lookup (one clone to hand out an
    /// owned value — the store itself is never copied).
    pub fn get(&self, id: &str) -> Result<Value> {
        if let Some(v2) = &self.v2 {
            return v2.get(id);
        }
        let docs = self.docs.read();
        docs.get(id)
            .map(|doc| (**doc).clone())
            .ok_or_else(|| Error::not_found(id))
    }

    /// Add one document to every index. Caller holds the writer lock.
    fn index_insert_doc(&self, id: &str, doc: &Value) {
        {
            let mut hash = self.hash_indexes.write();
            for (field, index) in hash.iter_mut() {
                if let Some(val) = field_get(doc, field) {
                    index.insert(val, id);
                }
            }
        }
        let mut btree = self.btree_indexes.write();
        for (field, index) in btree.iter_mut() {
            if let Some(val) = field_get(doc, field) {
                index.insert(val, id);
            }
        }
    }

    /// Remove one document from every index. Caller holds the writer lock.
    fn index_remove_doc(&self, id: &str, doc: &Value) {
        {
            let mut hash = self.hash_indexes.write();
            for (field, index) in hash.iter_mut() {
                if let Some(val) = field_get(doc, field) {
                    index.remove(val, id);
                }
            }
        }
        let mut btree = self.btree_indexes.write();
        for (field, index) in btree.iter_mut() {
            if let Some(val) = field_get(doc, field) {
                index.remove(val, id);
            }
        }
    }

    /// Move one document's index entries from its old value to its new one.
    fn index_update_doc(&self, id: &str, old_doc: &Value, new_doc: &Value) {
        {
            let mut hash = self.hash_indexes.write();
            for (field, index) in hash.iter_mut() {
                if let Some(old_val) = field_get(old_doc, field) {
                    index.remove(old_val, id);
                }
                if let Some(new_val) = field_get(new_doc, field) {
                    index.insert(new_val, id);
                }
            }
        }
        let mut btree = self.btree_indexes.write();
        for (field, index) in btree.iter_mut() {
            if let Some(old_val) = field_get(old_doc, field) {
                index.remove(old_val, id);
            }
            if let Some(new_val) = field_get(new_doc, field) {
                index.insert(new_val, id);
            }
        }
    }

    /// Sync secondary indexes after a document commit (delta ops:
    /// array_push / set / remove). Removes the old doc's indexed values,
    /// inserts the new ones. Caller must hold the writer lock, pass the
    /// doc's pre-commit snapshot, and commit the new doc to the store first.
    fn reindex_doc(&self, id: &str, old_doc: &Value) {
        let new_doc = self.docs.read().get(id).map(std::sync::Arc::clone);
        match new_doc {
            Some(new_doc) => self.index_update_doc(id, old_doc, &new_doc),
            None => self.index_remove_doc(id, old_doc),
        }

        // Full-text indexes: re-tokenize the field text.
        self.retext_doc(id, old_doc);
    }

    /// Sync full-text indexes after an in-place mutation. Caller holds the
    /// writer lock; `old_doc` is the pre-mutation snapshot.
    fn retext_doc(&self, id: &str, old_doc: &Value) {
        let mut text_indexes = self.text_indexes.write();
        if text_indexes.is_empty() {
            return;
        }
        let new_doc = self.docs.read().get(id).map(std::sync::Arc::clone);
        for (field, tindex) in text_indexes.iter_mut() {
            if old_doc.get(field).is_some()
                || new_doc.as_ref().map(|d| d.get(field).is_some()).unwrap_or(false)
            {
                match &new_doc {
                    Some(doc) => {
                        tindex.index_doc(id, doc);
                    }
                    None => tindex.remove_doc(id),
                }
            }
        }
    }

    /// Create a full-text index on a text field (opt-in, like hash/btree
    /// indexes). Boot path: load the disk cache if the journal is unchanged;
    /// otherwise build in parallel across all logical cores and cache the
    /// result. AND/OR/phrase/prefix queries then run via `text_search`.
    pub fn create_text_index(&self, field: &str) -> Result<()> {
        self.create_text_index_with_threads(field, 0)
    }

    /// Same as `create_text_index` with an explicit thread count for the
    /// (re)build. `threads = 0` means all logical cores. Ignored when the
    /// disk cache is fresh.
    pub fn create_text_index_with_threads(&self, field: &str, threads: usize) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.create_text_index(field, threads);
        }
        let _guard = self.writer.lock();

        // 1. Cache path: journal unchanged → load instead of building.
        if !self.is_in_memory() {
            let journal_size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
            let cache = self.base_dir.join(format!("_index_{}.fti", field));
            if let Some(tindex) = search::TextIndex::load(&cache, journal_size, field) {
                // Trust-but-verify: indexed-doc count must match the store's
                // docs-with-this-field, else the stamp lied.
                let with_field = self
                    .docs
                    .read()
                    .values()
                    .filter(|d| search::extract_text(d, field).is_some())
                    .count();
                if tindex.doc_count() == with_field {
                    self.text_indexes.write().insert(field.to_string(), tindex);
                    return Ok(());
                }
            }
        }

        // 2. Parallel build from (id, text) pairs.
        let docs_text: Vec<(String, String)> = {
            let docs = self.docs.read();
            docs.iter()
                .filter_map(|(id, doc)| {
                    search::extract_text(doc, field).map(|t| (id.clone(), t))
                })
                .collect()
        };
        let mut tindex = search::TextIndex::new(field);
        tindex.build_parallel(docs_text, threads);

        // 3. Cache for the next boot.
        if !self.is_in_memory() {
            let journal_size = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
            let cache = self.base_dir.join(format!("_index_{}.fti", field));
            // Cache write failure is non-fatal (read-only dir): search still works.
            if let Err(e) = tindex.save(&cache, journal_size) {
                report::suppressed("create_text_index: write disk cache", e);
            }
        }

        self.text_indexes.write().insert(field.to_string(), tindex);
        Ok(())
    }

    /// Drop a full-text index (and its disk cache, if any).
    pub fn drop_text_index(&self, field: &str) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.drop_text_index(field);
        }
        let _guard = self.writer.lock();
        self.text_indexes.write().remove(field);
        if !self.is_in_memory() {
            let cache = self.base_dir.join(format!("_index_{}.fti", field));
            // The load-time doc-count check catches a stale cache either way,
            // but failing to drop it is still said out loud.
            if cache.exists() {
                if let Err(e) = std::fs::remove_file(&cache) {
                    report::suppressed("drop_text_index: remove disk cache", e);
                }
            }
        }
        Ok(())
    }

    /// Run a full-text search over an indexed field. Returns matching _ids
    /// (unordered). The field must have a text index (fail loud otherwise).
    pub fn text_search(&self, field: &str, search: &TextSearch) -> Result<Vec<String>> {
        if let Some(v2) = &self.v2 {
            return v2.text_search(field, search);
        }
        search.validate()?;
        let text_indexes = self.text_indexes.read();
        let tindex = text_indexes
            .get(field)
            .ok_or_else(|| Error::invalid_arg(format!("no text index on '{}'", field)))?;
        let docs = self.docs.read();
        let ids = tindex.search(search, |id| {
            docs.get(id).and_then(|d| search::extract_text(d, field))
        });
        Ok(ids)
    }

    /// Update a document. Appends new version to file, old version superseded.
    /// O(1) operation.
    ///
    /// Ordering contract (#7): the journal append happens *before* any
    /// in-memory, index, ref-counter or file-bucket state is touched. A
    /// failed append therefore leaves every observable state exactly as it
    /// was and the caller may retry; everything after the append is
    /// infallible bookkeeping or reported cleanup.
    pub fn update(&self, id: &str, new_doc: Value) -> Result<()> {
        self.update_if(id, new_doc, None)
    }

    /// Update a document, optionally asserting its current seq.
    ///
    /// `if_seq` is the optimistic-concurrency token: `Some(n)` refuses with
    /// `Error::Conflict` (carrying the current document) unless the document
    /// is still at seq `n`. Checks and commit happen under one writer lock.
    pub fn update_if(&self, id: &str, mut new_doc: Value, if_seq: Option<u64>) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.update(id, new_doc, if_seq);
        }
        let _guard = self.writer.lock();

        let old_doc = {
            let docs = self.docs.read();
            match docs.get(id) {
                Some(doc) => std::sync::Arc::clone(doc),
                None => return Err(Error::not_found(id)),
            }
        };

        if let Some(expected) = if_seq {
            let current_seq = self.doc_seq.read().get(id).copied().unwrap_or(0);
            if expected != current_seq {
                return Err(Error::Conflict {
                    current_seq,
                    doc: (*old_doc).clone(),
                });
            }
        }

        // Set _id on new doc. Same caller-shape rule as `insert`.
        let object = new_doc
            .as_object_mut()
            .ok_or_else(|| Error::invalid_arg("document must be a JSON object"))?;
        object.insert("_id".to_string(), Value::String(id.to_string()));
        object.remove(storage::SEQ_FIELD);

        // Journal first: until the append succeeds, nothing else may move.
        let stored = std::sync::Arc::new(new_doc);
        self.commit_mutation(id, &stored, Some(std::sync::Arc::clone(&stored)))?;

        // Commit in-memory state: the write is durable, the rest is
        // infallible bookkeeping or reported cleanup.
        self.index_update_doc(id, &old_doc, &stored);
        self.docs
            .write()
            .insert(id.to_string(), std::sync::Arc::clone(&stored));

        // Full-text indexes (old_doc is the pre-update snapshot)
        self.retext_doc(id, &old_doc);

        // Ref-count delta and orphan cleanup, last: a file that fails to
        // move is under-cleaning — named, reported, swept by gc later.
        self.handle_ref_delta_and_trash(&old_doc, &stored);

        Ok(())
    }

    /// Append an element to an array field. O(1) file write.
    ///
    /// Dot-separated paths address nested arrays ("threads.2.messages");
    /// numeric segments index into arrays. A missing final key creates the array.
    ///
    /// Returns `Ok(true)` when the push landed. `Ok(false)` is a no-op, not a
    /// failure: the path did not resolve (missing intermediate, out-of-bounds
    /// index, non-array at the target) — nothing changed, nothing journaled.
    pub fn array_push(&self, id: &str, field: &str, value: Value) -> Result<bool> {
        if let Some(v2) = &self.v2 {
            return v2.array_push(id, field, value);
        }
        let _guard = self.writer.lock();

        let old_doc = {
            let docs = self.docs.read();
            match docs.get(id) {
                Some(doc) => std::sync::Arc::clone(doc),
                None => return Err(Error::not_found(id)),
            }
        };

        // Apply the path op to a snapshot: the stored document does not
        // move until the journal holds the patch (#7).
        let mut new_doc = (*old_doc).clone();
        if !apply_path_push(&mut new_doc, field, value.clone()) {
            // Unresolvable path: nothing mutated, so journal-first ordering
            // imposes nothing — return before the journal append (D1).
            return Ok(false);
        }

        // Journal first.
        let patch = serde_json::json!({
            "_id": id,
            "_op": "array_push",
            "path": field,
            "value": value
        });
        self.commit_doc_state(id, &old_doc, new_doc, &patch)?;

        Ok(true)
    }

    /// Journal a delta op, store its result, then sync indexes and ref
    /// counters. Journal-first ordering: nothing observable moves until the
    /// append succeeded (#7).
    fn commit_doc_state(
        &self,
        id: &str,
        old_doc: &Value,
        new_doc: Value,
        journal: &Value,
    ) -> Result<std::sync::Arc<Value>> {
        let stored = std::sync::Arc::new(new_doc);
        self.commit_mutation(id, journal, Some(std::sync::Arc::clone(&stored)))?;
        self.docs
            .write()
            .insert(id.to_string(), std::sync::Arc::clone(&stored));
        self.reindex_doc(id, old_doc);
        self.handle_ref_delta_and_trash(old_doc, &stored);
        Ok(stored)
    }

    /// Set a value at a dot-separated path within a document. O(1) file write.
    ///
    /// Path examples: "title", "messages.3.content", "settings.theme"
    /// Array indices are addressed by numeric path segments.
    ///
    /// Returns `Ok(true)` when the assignment landed — including a same-value
    /// re-assignment, which is an applied write, not a no-op. `Ok(false)`: the
    /// path did not resolve (missing intermediate segment, out-of-bounds
    /// index) — nothing changed, nothing journaled.
    pub fn set(&self, id: &str, path: &str, value: Value) -> Result<bool> {
        if let Some(v2) = &self.v2 {
            return v2.set(id, path, value);
        }
        let _guard = self.writer.lock();

        let old_doc = {
            let docs = self.docs.read();
            match docs.get(id) {
                Some(doc) => std::sync::Arc::clone(doc),
                None => return Err(Error::not_found(id)),
            }
        };

        let mut new_doc = (*old_doc).clone();
        if !apply_path_set(&mut new_doc, path, value.clone()) {
            // Unresolvable path: nothing mutated, so journal-first ordering
            // imposes nothing — return before the journal append (D1).
            return Ok(false);
        }

        // Journal first (#7).
        let patch = serde_json::json!({
            "_id": id,
            "_op": "set",
            "path": path,
            "value": value
        });
        self.commit_doc_state(id, &old_doc, new_doc, &patch)?;

        Ok(true)
    }

    /// Remove a field or array element at a dot-separated path. O(1) file write.
    ///
    /// Path examples: "title", "messages.3", "settings.theme"
    /// For array elements, the index is removed and the array shifts.
    ///
    /// Returns `Ok(true)` when something was removed. `Ok(false)` is a no-op:
    /// the path did not resolve (remove of a missing key is idempotent) —
    /// nothing changed, nothing journaled.
    pub fn remove(&self, id: &str, path: &str) -> Result<bool> {
        if let Some(v2) = &self.v2 {
            return v2.remove(id, path);
        }
        let _guard = self.writer.lock();

        let old_doc = {
            let docs = self.docs.read();
            match docs.get(id) {
                Some(doc) => std::sync::Arc::clone(doc),
                None => return Err(Error::not_found(id)),
            }
        };

        let mut new_doc = (*old_doc).clone();
        if !apply_path_remove(&mut new_doc, path) {
            // Unresolvable path: nothing mutated, so journal-first ordering
            // imposes nothing — return before the journal append (D1).
            return Ok(false);
        }

        // Journal first (#7).
        let patch = serde_json::json!({
            "_id": id,
            "_op": "remove",
            "path": path
        });
        self.commit_doc_state(id, &old_doc, new_doc, &patch)?;

        Ok(true)
    }

    /// The commit seq of a live document, `None` when it does not exist.
    pub fn doc_seq(&self, id: &str) -> Option<u64> {
        if let Some(v2) = &self.v2 {
            return v2.doc_seq(id);
        }
        self.doc_seq.read().get(id).copied()
    }

    /// Apply a batch of path operations as ONE atomic journal commit.
    ///
    /// All ops are applied to a staged copy under the writer lock; a single
    /// journal record (the full post-mutation document, carrying `_seq`) is
    /// appended, then the store, indexes and ref counters are updated once.
    /// A batch is all-or-nothing: if any op does not resolve (missing
    /// intermediate, out-of-bounds index, push onto a non-array) the call
    /// returns `Error::InvalidArgument` naming the op and **nothing is
    /// applied** — not to RAM, not to the journal (C3). This is deliberately
    /// stricter than the single-op `set`/`remove`/`array_push`, which report
    /// `Ok(false)` for a path that does not resolve.
    pub fn apply_ops(
        &self,
        id: &str,
        ops: &[PatchOp],
        if_seq: Option<u64>,
    ) -> Result<ApplyOpsResult> {
        if ops.is_empty() {
            return Err(Error::invalid_arg("patch must contain at least one op"));
        }
        if let Some(v2) = &self.v2 {
            return v2.apply_ops(id, ops.to_vec(), if_seq);
        }
        let _guard = self.writer.lock();

        let old_doc = {
            let docs = self.docs.read();
            match docs.get(id) {
                Some(doc) => std::sync::Arc::clone(doc),
                None => return Err(Error::not_found(id)),
            }
        };

        if let Some(expected) = if_seq {
            let current_seq = self.doc_seq.read().get(id).copied().unwrap_or(0);
            if expected != current_seq {
                return Err(Error::Conflict {
                    current_seq,
                    doc: (*old_doc).clone(),
                });
            }
        }

        let mut new_doc = (*old_doc).clone();
        let mut applied = Vec::with_capacity(ops.len());
        for (idx, op) in ops.iter().enumerate() {
            let ok = match op {
                PatchOp::Set { path, value } => apply_path_set(&mut new_doc, path, value.clone()),
                PatchOp::Remove { path } => apply_path_remove(&mut new_doc, path),
                PatchOp::ArrayPush { path, value } => {
                    apply_path_push(&mut new_doc, path, value.clone())
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

        // One journal record: the full post-mutation state, exactly like an
        // update — crash replay and `changes_since` need no patch engine.
        let stored = std::sync::Arc::new(new_doc);
        let seq = self.commit_mutation(id, &stored, Some(std::sync::Arc::clone(&stored)))?;

        self.docs
            .write()
            .insert(id.to_string(), std::sync::Arc::clone(&stored));
        self.reindex_doc(id, &old_doc);
        self.handle_ref_delta_and_trash(&old_doc, &stored);

        Ok(ApplyOpsResult {
            applied,
            seq,
            doc: (*stored).clone(),
        })
    }

    /// Helper to get the path of the persistent trash file.
    fn trash_doc_path(&self) -> PathBuf {        let filename = self.path.file_name().unwrap_or(std::ffi::OsStr::new("data.jsonl"));
        self.base_dir.join("_trash").join("docs").join(filename)
    }

    /// Delete a document by ID. O(1) duration.
    /// In an on-disk database, writes a tombstone instead of deleting data.
    pub fn delete(&self, id: &str) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.delete(id);
        }
        let _guard = self.writer.lock();

        let doc_to_trash = {
            let docs = self.docs.read();
            match docs.get(id) {
                Some(doc) => std::sync::Arc::clone(doc),
                None => return Err(Error::not_found(id)),
            }
        };

        // The file refs this document holds, and which of them losing it would
        // orphan. Decided by *reading* the counters, not by mutating them, so
        // that a failure before the commit below leaves the counters and the
        // document store exactly as they were.
        let mut extracted_file_refs = HashSet::new();
        Self::extract_file_refs(&doc_to_trash, &mut extracted_file_refs);
        let orphaned_files: Vec<String> = {
            let file_refs = self.file_refs.read();
            extracted_file_refs
                .iter()
                .filter(|r| file_refs.get(*r).map_or(false, |count| *count <= 1))
                .cloned()
                .collect()
        };

        // Bucket policy (#10): a bucket declared `onDocumentDelete: "restrict"`
        // refuses the delete while the document references its files — the
        // consumer must reassign or explicitly release the file first. This
        // is a precondition, checked before anything moves, so the refusal
        // leaves document, counters and files exactly as they were.
        for r in &extracted_file_refs {
            if let Some(file_ref) = FileRef::from_compact(r) {
                if let Some(policy) = self.bucket_policies.get(&file_ref.bucket) {
                    if policy.on_document_delete == Some(OnDocumentDelete::Restrict) {
                        return Err(Error::policy_violation(format!(
                            "delete refused: bucket '{}' declares onDocumentDelete=restrict; reassign or release {} first",
                            file_ref.bucket, r
                        )));
                    }
                }
            }
        }

        // The trash record *is* the restorability guarantee: a caller told
        // "deleted" must be able to find the document again in the trash. So
        // writing it is a precondition of the delete, not best-effort cleanup —
        // if it cannot be written, the delete does not happen and the caller
        // may retry, rather than being told a restorable deletion occurred.
        if !self.is_in_memory() && self.trash_mode != TrashMode::Off {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let mut trash_doc = (*doc_to_trash).clone();
            if let Some(obj) = trash_doc.as_object_mut() {
                obj.insert("_deleted".to_string(), serde_json::json!(now));
                if !orphaned_files.is_empty() {
                    obj.insert(
                        "_trashed_files".to_string(),
                        serde_json::to_value(&orphaned_files)?,
                    );
                }
            }
            // B3: without this lock a concurrent purge (foreground or TTL
            // thread) can read the trash file before this append lands and
            // rewrite it after — erasing the very record this delete is
            // about to guarantee.
            let _trash_guard = self.trash_lock.lock();
            storage::append_doc_trash(&self.trash_doc_path(), &trash_doc)?;
        }

        // The deletion commit, ordered *before* any destructive file move so
        // that every prefix of this operation leaves a state that is at worst
        // under-cleaned: a document whose files are still sitting in the active
        // bucket is swept up later by gc_buckets, whereas a document that is
        // still live while its files have already moved to trash is a dangling
        // reference. Reaching here means the restorable copy is already on
        // disk, so a failure now cannot strand an un-restorable delete.
        {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let tombstone = serde_json::json!({
                "_id": id,
                "_deleted": now
            });
            self.commit_mutation(id, &tombstone, None)?;
        }

        // Now retire the ref counts and move the orphans aside. A file that
        // fails to move stays where it is and is named: leaving it behind is
        // recoverable (the GC sweeps it later, and the document is honestly
        // gone), whereas a silent failure would hide a stranded file for the
        // lifetime of the database.
        {
            let mut file_refs = self.file_refs.write();
            for r in &extracted_file_refs {
                if let Some(count) = file_refs.get_mut(r) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        file_refs.remove(r);
                    }
                }
            }
        }
        for f in &orphaned_files {
            if let Some(file_ref) = FileRef::from_compact(f) {
                let bucket = self.bucket(&file_ref.bucket);
                if let Err(e) = bucket.delete(&file_ref) {
                    report::suppressed("delete: move orphaned file to trash", e);
                }
            }
        }

        // Remove from indexes
        self.index_remove_doc(id, &doc_to_trash);

        // Remove from full-text indexes (doc is about to leave the store;
        // retext_doc would re-see it, so purge directly)
        {
            let mut text_indexes = self.text_indexes.write();
            for (_, tindex) in text_indexes.iter_mut() {
                tindex.remove_doc(id);
            }
        }

        // Update in-memory state
        self.docs.write().remove(id);
        self.deleted.write().insert(id.to_string());

        // Handle trash mode
        if self.trash_mode == TrashMode::Off {
            // Hard delete — nothing to keep
        }

        Ok(())
    }

    /// Iterator over all non-deleted documents.
    /// Returns owned `Value`s (one clone per document) for thread safety.
    pub fn iter(&self) -> Vec<Value> {
        if let Some(v2) = &self.v2 {
            return v2
                .iter()
                .unwrap_or_else(|e| panic!("v2 iter: {e}"));
        }
        let docs = self.docs.read();
        docs.values().map(|doc| (**doc).clone()).collect()
    }

    /// Number of active (non-deleted) documents.
    pub fn len(&self) -> usize {
        if let Some(v2) = &self.v2 {
            return v2.len();
        }
        self.docs.read().len()
    }

    /// Check if database is empty.
    pub fn is_empty(&self) -> bool {
        if let Some(v2) = &self.v2 {
            return v2.is_empty();
        }
        self.docs.read().is_empty()
    }

    /// Check if a document exists.
    pub fn contains(&self, id: &str) -> bool {
        if let Some(v2) = &self.v2 {
            return v2.contains(id);
        }
        self.docs.read().contains_key(id)
    }

    // ─── Layer 2: Single Field Queries ─────────────────────────────

    /// Find all documents where `field` equals `value`.
    /// Uses an index if available, otherwise linear scan. The index only
    /// narrows the candidates — every candidate is re-checked with the same
    /// whole-value equality the scan uses, so both paths return the same set.
    pub fn find(&self, field: &str, value: &Value) -> Vec<Value> {
        if let Some(v2) = &self.v2 {
            return v2.find(field, value).unwrap_or_else(|e| panic!("v2 find: {e}"));
        }
        if let Some(ids) = self.index_lookup(field, value) {
            let docs = self.docs.read();
            let mut ids: Vec<String> = ids.into_iter().collect();
            ids.sort();
            return ids
                .into_iter()
                .filter_map(|id| docs.get(&id).map(std::sync::Arc::clone))
                .filter(|doc| {
                    doc.get(field)
                        .map(|v| values_equal(v, value))
                        .unwrap_or(false)
                })
                .map(|doc| (*doc).clone())
                .collect();
        }

        // Linear scan
        let docs = self.docs.read();
        docs.values()
            .filter(|doc| {
                doc.get(field)
                    .map(|v| values_equal(v, value))
                    .unwrap_or(false)
            })
            .map(|doc| (**doc).clone())
            .collect()
    }

    /// Find documents where field matches a predicate closure.
    pub fn find_where<F>(&self, field: &str, predicate: F) -> Vec<Value>
    where
        F: Fn(&Value) -> bool,
    {
        if let Some(v2) = &self.v2 {
            return v2
                .find_where(field, predicate)
                .unwrap_or_else(|e| panic!("v2 find_where: {e}"));
        }
        let docs = self.docs.read();
        docs.values()
            .filter(|doc| {
                doc.get(field)
                    .map(|v| predicate(v))
                    .unwrap_or(false)
            })
            .map(|doc| (**doc).clone())
            .collect()
    }

    /// Find documents with field value in the closed range `[min, max]`.
    ///
    /// Uses the BTree index when one exists on `field` — `BTreeMap::range`
    /// over the shared [`OrderedValue`] key order, which is exactly the
    /// comparison a linear scan applies, so both paths are equal sets.
    pub fn find_range(&self, field: &str, min: &Value, max: &Value) -> Vec<Value> {
        if let Some(v2) = &self.v2 {
            return v2
                .find_range(field, min, max)
                .unwrap_or_else(|e| panic!("v2 find_range: {e}"));
        }
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
            let docs = self.docs.read();
            let mut ids: Vec<String> = ids.into_iter().collect();
            ids.sort();
            return ids
                .into_iter()
                .filter_map(|id| docs.get(&id).map(std::sync::Arc::clone))
                .filter(|doc| in_range(doc))
                .map(|doc| (*doc).clone())
                .collect();
        }

        let docs = self.docs.read();
        docs.values()
            .filter(|doc| in_range(doc))
            .map(|doc| (**doc).clone())
            .collect()
    }

    // ─── Query Planner ─────────────────────────────────────────────

    /// Exact key lookup in whichever index covers `field`.
    fn index_lookup(&self, field: &str, value: &Value) -> Option<HashSet<String>> {
        {
            let hash = self.hash_indexes.read();
            if let Some(index) = hash.get(field) {
                return Some(index.get(value).into_iter().collect());
            }
        }
        let btree = self.btree_indexes.read();
        btree
            .get(field)
            .map(|index| index.get(value).into_iter().collect())
    }

    /// Ordered range scan; `None` when `field` has no BTree index.
    fn index_range(
        &self,
        field: &str,
        min: KeyBound<'_>,
        max: KeyBound<'_>,
    ) -> Option<HashSet<String>> {
        let btree = self.btree_indexes.read();
        let index = btree.get(field)?;
        index
            .range(min, max)
            .map(|ids| ids.into_iter().collect())
    }

    /// `$in` lookup: the union of exact lookups for each element.
    fn index_in(&self, field: &str, operand: &Value) -> Option<HashSet<String>> {
        let values = operand.as_array()?;
        if values.is_empty() {
            // `$in: []` matches nothing — an exact empty candidate set, so
            // only report it when an index actually covers the field.
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
        if indexed {
            Some(out)
        } else {
            None
        }
    }

    /// Candidate ids for `ast` (the shared planner).
    fn plan_candidates(&self, ast: &Value) -> Option<HashSet<String>> {
        plan_candidates(ast, self)
    }

    /// Every document matching `ast`, as shared references: matching itself
    /// copies no document, and the returned handles keep the values alive
    /// after the store lock is released.
    fn collect_matches(
        &self,
        ast: &Value,
        text_ids: Option<&HashSet<String>>,
    ) -> Vec<std::sync::Arc<Value>> {
        let text_ok = |id: &str| text_ids.map_or(true, |set| set.contains(id));
        let candidates = self.plan_candidates(ast);
        let docs = self.docs.read();

        match candidates {
            Some(ids) => {
                let mut ids: Vec<&String> = ids.iter().collect();
                ids.sort();
                ids.into_iter()
                    .filter_map(|id| docs.get(id).map(|doc| (id, doc)))
                    .filter(|(id, doc)| {
                        text_ok(id) && query_matches(doc.as_ref(), ast)
                    })
                    .map(|(_, doc)| std::sync::Arc::clone(doc))
                    .collect()
            }
            None => docs
                .iter()
                .filter(|(id, doc)| text_ok(id) && query_matches(doc.as_ref(), ast))
                .map(|(_, doc)| std::sync::Arc::clone(doc))
                .collect(),
        }
    }

    // ─── Layer 3: JSON AST Queries ─────────────────────────────────

    /// Execute a JSON AST query. Returns all matching documents.
    ///
    /// The AST is validated first (G4): an unknown operator or malformed
    /// combinator is `Err(InvalidArgument)` — on every path, not just HTTP.
    /// A typo must fail loud, never silently degrade to a full-DB scan.
    ///
    /// Indexable conjuncts narrow the scan first (the query planner); the
    /// result is identical to a full scan either way.
    pub fn query(&self, ast: Value) -> Result<Vec<Value>> {
        if let Some(v2) = &self.v2 {
            return v2.query(ast);
        }
        validate_query_ast(&ast)?;
        Ok(self
            .collect_matches(&ast, None)
            .into_iter()
            .map(|doc| (*doc).clone())
            .collect())
    }

    /// Execute a JSON AST query with options (limit, sort, offset).
    /// Validated like `query` (G4).
    pub fn query_with(&self, ast: Value, opts: QueryOptions) -> Result<Vec<Value>> {
        if let Some(v2) = &self.v2 {
            return v2.query_with(ast, opts);
        }
        let mut results = self.query(ast)?;

        // Sort (dot-notation aware — "meta.views" resolves, not just top-level)
        if let Some((ref field, dir)) = opts.sort_by {
            results.sort_by(|a, b| {
                let av = field_get(a, field).unwrap_or(&Value::Null);
                let bv = field_get(b, field).unwrap_or(&Value::Null);
                let cmp = value_cmp(av, bv);
                match dir {
                    SortDir::Asc => cmp,
                    SortDir::Desc => cmp.reverse(),
                }
            });
        }

        // Offset
        let offset = opts.offset.unwrap_or(0);
        if offset > 0 {
            results = results.into_iter().skip(offset).collect();
        }

        // Limit
        if let Some(limit) = opts.limit {
            results.truncate(limit);
        }

        Ok(results)
    }

    /// Execute a query returning only the projected fields, cloning the
    /// minimum: matching collects shared handles only, so a non-matching
    /// document allocates nothing and a projected result clones only the
    /// requested fields. Sort/offset/limit run after the store lock is
    /// released, so writers are never blocked by materialization of the
    /// result page. `_id` is always included.
    ///
    /// Returns `(total, page)` where `total` is the pre-pagination match count.
    ///
    /// `fields: None` returns full documents (same as query_with).
    /// `text_ids: Some(set)` additionally requires the doc id to be in the
    /// set — the intersection of a full-text candidate set with the filter.
    /// `None` = no text constraint (zero overhead).
    pub fn query_projected(
        &self,
        ast: Value,
        opts: QueryOptions,
        fields: Option<&[String]>,
        text_ids: Option<&HashSet<String>>,
    ) -> Result<(usize, Vec<Value>)> {
        if let Some(v2) = &self.v2 {
            return v2.query_projected(ast, opts, fields, text_ids);
        }
        validate_query_ast(&ast)?;
        let sort_field = opts.sort_by.as_ref().map(|(f, _)| f.clone());
        let matches = self.collect_matches(&ast, text_ids);

        let mut rows: Vec<(Value, Value)> = matches
            .iter()
            .map(|doc| {
                let key = match &sort_field {
                    Some(f) => field_get(doc, f).cloned().unwrap_or(Value::Null),
                    None => Value::Null,
                };
                let proj = match fields {
                    Some(fs) => project_doc(doc, fs),
                    None => (**doc).clone(),
                };
                (key, proj)
            })
            .collect();

        if let Some((_, dir)) = &opts.sort_by {
            rows.sort_by(|a, b| match dir {
                SortDir::Asc => value_cmp(&a.0, &b.0),
                SortDir::Desc => value_cmp(&a.0, &b.0).reverse(),
            });
        }

        let total = rows.len();

        let offset = opts.offset.unwrap_or(0);
        if offset > 0 {
            rows.drain(..offset.min(rows.len()));
        }
        if let Some(limit) = opts.limit {
            rows.truncate(limit);
        }

        Ok((total, rows.into_iter().map(|(_, proj)| proj).collect()))
    }

    /// The selected page as pre-serialized JSON bytes, for the HTTP layer.
    ///
    /// On v2 the bytes come straight from the store (or the mapping) instead
    /// of being parsed into a `Value` and re-serialized per hit; on v1 the
    /// documents are serialized once. Same selection, sort and pagination as
    /// [`Database::query_projected`].
    pub fn query_raw(
        &self,
        ast: Value,
        opts: QueryOptions,
        text_ids: Option<&HashSet<String>>,
    ) -> Result<(usize, Vec<std::sync::Arc<[u8]>>)> {
        if let Some(v2) = &self.v2 {
            return v2.query_raw(ast, opts, text_ids);
        }
        let (total, docs) = self.query_projected(ast, opts, None, text_ids)?;
        let mut out = Vec::with_capacity(docs.len());
        for doc in docs {
            out.push(std::sync::Arc::from(serde_json::to_vec(&doc)?));
        }
        Ok((total, out))
    }

    /// Term counts per field, single O(n) scan. Array fields count per element
    /// (each category in `categories` counts once); scalar fields count per
    /// value. Only string values are counted (numbers/bools are skipped).
    /// Returns (field, counts) pairs in input order; counts are in BTreeMap
    /// (sorted) order for deterministic output.
    pub fn facet_counts(&self, fields: &[String]) -> Vec<(String, BTreeMap<String, usize>)> {
        if let Some(v2) = &self.v2 {
            return v2
                .facet_counts(fields)
                .unwrap_or_else(|e| panic!("v2 facet_counts: {e}"));
        }
        let mut counts: Vec<(String, BTreeMap<String, usize>)> =
            fields.iter().map(|f| (f.clone(), BTreeMap::new())).collect();
        {
            let docs = self.docs.read();
            for doc in docs.values() {
                for (field, map) in counts.iter_mut() {
                    let Some(val) = doc.get(field.as_str()) else { continue };
                    let terms: Vec<&str> = match val {
                        Value::String(s) => vec![s.as_str()],
                        Value::Array(arr) => arr.iter().filter_map(|x| x.as_str()).collect(),
                        _ => continue,
                    };
                    for t in terms {
                        *map.entry(t.to_string()).or_insert(0) += 1;
                    }
                }
            }
        }
        counts
    }

    // ─── Index Management ──────────────────────────────────────────

    /// Create a hash index on a field (one scan of the store). Dot-notation
    /// paths resolve exactly like `field_get`, so `meta.score` is indexable.
    pub fn create_index(&self, field: &str) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.create_index(field, V2IndexKind::Hash);
        }
        let _guard = self.writer.lock();

        let mut index = HashIndex::new();
        let docs = self.docs.read();
        for (id, doc) in docs.iter() {
            if let Some(val) = field_get(doc, field) {
                index.insert(val, id);
            }
        }

        self.hash_indexes
            .write()
            .insert(field.to_string(), index);
        Ok(())
    }

    /// Create a BTree index on a field (ordered equality + range scans).
    /// A field may carry a hash index and a BTree index at the same time.
    pub fn create_btree_index(&self, field: &str) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.create_index(field, V2IndexKind::BTree);
        }
        let _guard = self.writer.lock();

        let mut index = BTreeIndex::new();
        let docs = self.docs.read();
        for (id, doc) in docs.iter() {
            if let Some(val) = field_get(doc, field) {
                index.insert(val, id);
            }
        }

        self.btree_indexes
            .write()
            .insert(field.to_string(), index);
        Ok(())
    }

    /// Drop every index on a field, freeing memory.
    pub fn drop_index(&self, field: &str) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.drop_index(field);
        }
        let hash = self.hash_indexes.write().remove(field).is_some();
        let btree = self.btree_indexes.write().remove(field).is_some();
        if !hash && !btree {
            return Err(Error::index_error(field, "index not found"));
        }
        Ok(())
    }

    /// Check if any index exists for a field.
    pub fn has_index(&self, field: &str) -> bool {
        if let Some(v2) = &self.v2 {
            return v2.has_index(field);
        }
        self.hash_indexes.read().contains_key(field)
            || self.btree_indexes.read().contains_key(field)
    }

    /// Check if a full-text index exists for a field.
    pub fn has_text_index(&self, field: &str) -> bool {
        if let Some(v2) = &self.v2 {
            return v2.has_text_index(field);
        }
        self.text_indexes.read().contains_key(field)
    }

    // ─── Compaction & Trash ────────────────────────────────────────

    /// Compact the database: rewrite active docs to a single file and discard any tombstones.
    ///
    /// The journal identity and `last_seq` survive — `seq` is never reset — and
    /// `min_available_seq` advances to `last_seq + 1`: everything before it was
    /// rewritten as base state and can no longer be replayed as events.
    pub fn compact(&self) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.compact();
        }
        let _guard = self.writer.lock();

        let lineage = self.lineage.read().clone();
        let next_min = lineage.last_seq.saturating_add(1);

        if !self.is_in_memory() {
            // Close file handle before rewrite
            {
                let mut handle = self.file_handle.lock();
                *handle = None;
            }

            let docs = self.docs.read();
            let active: Vec<&Value> = docs.values().map(|doc| doc.as_ref()).collect();

            // Rewrite active docs. Tombstones in the old data.jsonl are permanently dropped,
            // which is safe because `delete()` already archived the full documents into
            // the persistent `_trash/docs/{dbname}.jsonl` file.
            storage::rewrite_atomic_with_meta(
                &self.path,
                &active,
                &SourceLineage {
                    source_epoch_uuid: lineage.source_epoch_uuid,
                    last_seq: lineage.last_seq,
                    min_available_seq: next_min,
                },
            )?;
        }

        // Only after the rewrite is durable: history before the new watermark
        // is gone either way, so say so once the base state exists.
        self.lineage.write().min_available_seq = next_min;
        self.change_log.write().clear();

        Ok(())
    }

    // ─── Sync Feed ─────────────────────────────────────────────────

    /// Current journal identity and seq watermarks.
    pub fn lineage(&self) -> SourceLineage {
        if let Some(v2) = &self.v2 {
            return v2.lineage();
        }
        self.lineage.read().clone()
    }

    /// A consistent `(lineage, docs)` pair: both are read under the writer
    /// lock, so no commit can land between the watermark and the document set.
    /// Documents are sorted by id.
    pub fn snapshot_at(&self) -> Snapshot {
        if let Some(v2) = &self.v2 {
            return v2
                .snapshot_at()
                .unwrap_or_else(|e| panic!("v2 snapshot_at: {e}"));
        }
        let _guard = self.writer.lock();
        let lineage = self.lineage.read().clone();
        let docs = self.docs.read();
        let mut out: Vec<(String, Value)> = docs
            .iter()
            .map(|(id, doc)| (id.clone(), (**doc).clone()))
            .collect();
        out.sort_by(|a, b| a.0.cmp(&b.0));
        Snapshot { lineage, docs: out }
    }

    /// Every committed change with `seq > since_seq`, ascending.
    ///
    /// `CursorAhead` means the caller holds a watermark this journal never
    /// issued (a backup from a different lineage). `CursorExpired` means the
    /// events after `since_seq` were rewritten away by a compaction and the
    /// caller must resync from a fresh snapshot.
    pub fn changes_since(&self, since_seq: u64) -> Result<ChangeBatch> {
        if let Some(v2) = &self.v2 {
            return v2.changes_since(since_seq);
        }
        let _guard = self.writer.lock();
        let lineage = self.lineage.read().clone();

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

        let events: Vec<ChangeEvent> = self
            .change_log
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
        Ok(ChangeBatch { lineage, events })
    }

    /// Purge documents from the persistent trash file and files from the file trash 
    /// that are older than the configured TTL (or all if duration is ZERO).
    pub fn purge_trash(&self) -> Result<usize> {
        // Held for the whole sweep: the doc-trash read-all→rewrite must not
        // interleave with delete()'s record append (B3).
        let _trash_guard = self.trash_lock.lock();
        let ttl = match (self.trash_mode, self.trash_ttl) {
            (TrashMode::TTL(t), _) => t,
            (_, Some(t)) => t,
            _ => Duration::ZERO,
        };

        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();
        let cutoff = now.saturating_sub(ttl.as_secs());

        // Purge Document Trash
        let mut purged_count = 0;
        let trash_file = self.trash_doc_path();
        if trash_file.exists() {
            let all_trash = storage::read_trash(&trash_file)?;
            let mut keep: Vec<&Value> = Vec::new();
            for doc in &all_trash {
                if let Some(deleted) = doc.get("_deleted").and_then(|v| v.as_u64()) {
                    if deleted > cutoff {
                        keep.push(doc); // Still active in trash
                    } else {
                        purged_count += 1; // Passed TTL
                    }
                } else {
                    keep.push(doc); // Keep malformed safe
                }
            }
            if purged_count > 0 {
                storage::rewrite_atomic(&trash_file, &keep)?;
            }
        }

        // Purge File Trash in all buckets using Folder-Led Purging
        let trash_files_dir = self.base_dir.join("_trash").join("files");
        if trash_files_dir.exists() {
            // Nothing here raises: every failure is named and skipped, and the
            // count the caller gets back covers only what was really removed.
            match std::fs::read_dir(&trash_files_dir) {
                Ok(entries) => {
                    for entry in entries {
                        let entry = match entry {
                            Ok(entry) => entry,
                            Err(e) => {
                                report::suppressed("purge_trash: read bucket trash entry", e);
                                continue;
                            }
                        };
                        let file_type = match entry.file_type() {
                            Ok(file_type) => file_type,
                            Err(e) => {
                                report::suppressed("purge_trash: stat bucket trash entry", e);
                                continue;
                            }
                        };
                        if file_type.is_dir() {
                            let name = entry.file_name().to_string_lossy().to_string();
                            let bucket = self.bucket(&name);
                            // Per-bucket TTL from meta.json overrides the
                            // database-wide one for this bucket's trash (#10).
                            let bucket_ttl = self
                                .bucket_policies
                                .get(&name)
                                .and_then(|p| p.ttl_seconds)
                                .map(Duration::from_secs)
                                .unwrap_or(ttl);
                            if let Err(e) = bucket.purge_trash_ttl(bucket_ttl) {
                                report::suppressed("purge_trash: purge bucket trash", e);
                            }
                        }
                    }
                }
                Err(e) => report::suppressed(
                    "purge_trash: list bucket trash directory",
                    format!("{}: {e}", trash_files_dir.display()),
                ),
            }
        }

        Ok(purged_count)
    }

    /// Export a pristine snapshot of the database to a target directory.
    /// Functions similarly to compaction, but writes exactly the active 
    /// docs, metadata, and files to a completely new folder without trash.
    pub fn export_snapshot<P: AsRef<Path>>(&self, target_dir: P) -> Result<()> {
        let _guard = self.writer.lock();
        let target = target_dir.as_ref();
        fs::create_dir_all(target).map_err(Error::io_err(target, "create snapshot directory"))?;

        if let Some(v2) = &self.v2 {
            // The engine writes the clean `data.jsonl`; meta.json and buckets
            // are format-agnostic and shared with the v1 path below.
            v2.export_snapshot(target)?;
            self.copy_snapshot_extras(target)?;
            return Ok(());
        }

        let docs = self.docs.read();
        let active: Vec<&Value> = docs.values().map(|doc| doc.as_ref()).collect();
        
        // 1. Write the clean JSON line docs — the epoch and seq watermark carry
        //    over; only the replay window resets to what this snapshot holds.
        let target_db = target.join(
            self.path
                .file_name()
                .ok_or_else(|| Error::invalid_arg("database path has no file name"))?,
        );
        let lineage = self.lineage.read().clone();
        storage::rewrite_atomic_with_meta(
            &target_db,
            &active,
            &SourceLineage {
                source_epoch_uuid: lineage.source_epoch_uuid,
                min_available_seq: lineage.last_seq.saturating_add(1),
                last_seq: lineage.last_seq,
            },
        )?;
        drop(docs);

        // 2 + 3. meta.json and the active bucket files.
        self.copy_snapshot_extras(target)?;

        Ok(())
    }

    /// Copy `meta.json` and the live bucket files into an exported snapshot.
    fn copy_snapshot_extras(&self, target: &Path) -> Result<()> {
        let meta_src = self.base_dir.join("meta.json");
        let meta_dst = target.join("meta.json");
        if meta_src.exists() {
            fs::copy(&meta_src, &meta_dst)
                .map_err(Error::io_err(&meta_dst, "copy meta.json for snapshot"))?;
        }

        let buckets_src = self.base_dir.join("_files");
        let buckets_dst = target.join("_files");
        if buckets_src.exists() {
            // Find only buckets natively active in this DB
            for entry in
                fs::read_dir(&buckets_src).map_err(Error::io_err(&buckets_src, "read buckets dir"))?
            {
                let entry = entry.map_err(Error::io_err(&buckets_src, "read bucket entry"))?;
                if entry.file_type().map_or(false, |t| t.is_dir()) {
                    let bucket_name = entry.file_name();
                    let dst_bucket = buckets_dst.join(&bucket_name);
                    fs::create_dir_all(&dst_bucket)
                        .map_err(Error::io_err(&dst_bucket, "create snapshot bucket"))?;

                    // Copy non-trash binaries; item buckets (#9) store one
                    // folder per item, copied whole.
                    for file_entry in fs::read_dir(entry.path())
                        .map_err(Error::io_err(&entry.path(), "read bucket files"))?
                    {
                        let f = file_entry
                            .map_err(Error::io_err(&entry.path(), "read active binary"))?;
                        let file_name = f.file_name();
                        if file_name == "_trash" {
                            continue;
                        }
                        if f.file_type().map_or(false, |t| t.is_file()) {
                            fs::copy(f.path(), dst_bucket.join(&file_name))
                                .map_err(Error::io_err(f.path(), "copy file to snapshot"))?;
                        } else if f.file_type().map_or(false, |t| t.is_dir()) {
                            copy_dir_recursive(&f.path(), &dst_bucket.join(&file_name))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }

    /// Every live document as an owned `(id, doc)` pair.
    pub fn all_docs(&self) -> Result<Vec<(String, Value)>> {
        if let Some(v2) = &self.v2 {
            return v2.all_docs();
        }
        let docs = self.docs.read();
        Ok(docs
            .iter()
            .map(|(id, doc)| (id.clone(), (**doc).clone()))
            .collect())
    }

    /// Write the database as human-readable JSON Lines (a `_meta` header when
    /// `with_meta`), sorted by id. Returns the number of documents written.
    pub fn export_jsonl<P: AsRef<Path>>(&self, out: P, with_meta: bool) -> Result<usize> {
        if let Some(v2) = &self.v2 {
            return v2.export_jsonl(out.as_ref(), with_meta);
        }
        let out = out.as_ref();
        let lineage = self.lineage();
        let mut docs = self.all_docs()?;
        docs.sort_by(|a, b| a.0.cmp(&b.0));

        let mut buf = String::new();
        if with_meta {
            let meta = serde_json::json!({
                "_meta": {
                    "version": 1,
                    "created": now_secs(),
                    "source_epoch_uuid": lineage.source_epoch_uuid,
                    "last_seq": lineage.last_seq,
                    "min_available_seq": lineage.last_seq.saturating_add(1),
                }
            });
            buf.push_str(&meta.to_string());
            buf.push('\n');
        }
        for (_, doc) in &docs {
            buf.push_str(&serde_json::to_string(doc)?);
            buf.push('\n');
        }
        if let Some(parent) = out.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(Error::io_err(parent, "create export directory"))?;
            }
        }
        fs::write(out, buf).map_err(Error::io_err(out, "write JSONL export"))?;
        Ok(docs.len())
    }

    /// Get the trash directory path.
    pub fn trash_dir(&self) -> PathBuf {
        self.base_dir.join("_trash").join("docs")
    }

    /// List deleted document IDs.
    pub fn deleted_ids(&self) -> Vec<String> {
        if let Some(v2) = &self.v2 {
            return v2.deleted_ids();
        }
        self.deleted.read().iter().cloned().collect()
    }

    /// Get all active document IDs.
    pub fn get_all_ids(&self) -> Vec<String> {
        if let Some(v2) = &self.v2 {
            return v2.get_all_ids();
        }
        self.docs.read().keys().cloned().collect()
    }

    /// Restore a deleted document from trash by ID.
    pub fn restore(&self, id: &str) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return self.restore_v2(v2, id);
        }
        let _guard = self.writer.lock();

        if self.is_in_memory() {
            return Err(Error::invalid_arg("cannot restore in in-memory database"));
        }

        // G1: only a currently-deleted document can be restored. The trash
        // file keeps entries after a restore, so a second restore would
        // re-return files and re-inflate the ref counters for a document
        // that is already live.
        if !self.deleted.read().contains(id) {
            return Err(Error::invalid_arg(format!("restore: {id} is not deleted")));
        }

        // Read TRASH file to find the most recent trash entry for this ID
        let trash_docs = storage::read_trash(&self.trash_doc_path())?;
        let mut last_trash_entry: Option<Value> = None;
        for doc in trash_docs.into_iter().rev() {
            if doc.get("_id").and_then(|v| v.as_str()) == Some(id) {
                last_trash_entry = Some(doc);
                break;
            }
        }

        let mut trash_doc = last_trash_entry.ok_or_else(|| Error::not_found(id))?;

        // The file list is needed after the metadata strip below — owned,
        // because the strip mutates the document it came from.
        let trashed_files: Vec<String> = trash_doc
            .get("_trashed_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        // Strictly strip engine metadata
        if let Some(obj) = trash_doc.as_object_mut() {
            obj.remove("_deleted");
            obj.remove("_trashed_files");
        }

        let doc = trash_doc;

        // Journal first (G1): the restored document is appended before any
        // file, counter, or in-memory state moves, so a failed append leaves
        // everything exactly as it was and the restore is retryable.
        let stored = std::sync::Arc::new(doc);
        self.commit_mutation(id, &stored, Some(std::sync::Arc::clone(&stored)))?;

        // Return any implicitly trashed files — after the journal holds the
        // restored document. The document comes back either way; a file that
        // did not is a dangling reference, which the caller must be able to
        // see rather than have silently re-declared as restored.
        for s in &trashed_files {
            if let Some(file_ref) = FileRef::from_compact(s) {
                let bucket = self.bucket(&file_ref.bucket);
                if let Err(e) = bucket.restore(&file_ref.id, &file_ref.ext) {
                    report::suppressed("restore: return file from trash", e);
                }
            }
        }

        // Restore file reference counters
        let mut extracted_file_refs = HashSet::new();
        Self::extract_file_refs(&stored, &mut extracted_file_refs);
        {
            let mut file_refs = self.file_refs.write();
            for r in extracted_file_refs {
                *file_refs.entry(r).or_insert(0) += 1;
            }
        }

        // Update in-memory state
        self.deleted.write().remove(id);
        self.docs.write().insert(id.to_string(), stored);

        Ok(())
    }

    /// Restore a deleted document from the v2 trash file.
    fn restore_v2(&self, v2: &Arc<v2_engine::V2Engine>, id: &str) -> Result<()> {
        let _guard = self.writer.lock();

        if !v2.is_deleted(id) {
            return Err(Error::invalid_arg(format!("restore: {id} is not deleted")));
        }

        let trash_docs = storage::read_trash(&self.trash_doc_path())?;
        let mut last_trash_entry: Option<Value> = None;
        for doc in trash_docs.into_iter().rev() {
            if doc.get("_id").and_then(|v| v.as_str()) == Some(id) {
                last_trash_entry = Some(doc);
                break;
            }
        }
        let mut trash_doc = last_trash_entry.ok_or_else(|| Error::not_found(id))?;

        let trashed_files: Vec<String> = trash_doc
            .get("_trashed_files")
            .and_then(|v| v.as_array())
            .map(|a| {
                a.iter()
                    .filter_map(|v| v.as_str().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default();

        if let Some(obj) = trash_doc.as_object_mut() {
            obj.remove("_deleted");
            obj.remove("_trashed_files");
        }

        // Journal (the WAL put) first: a failed commit leaves the document
        // deleted and the restore retryable.
        v2.restore(id, trash_doc)?;

        for s in &trashed_files {
            if let Some(file_ref) = FileRef::from_compact(s) {
                let bucket = self.bucket(&file_ref.bucket);
                if let Err(e) = bucket.restore(&file_ref.id, &file_ref.ext) {
                    report::suppressed("restore: return file from trash", e);
                }
            }
        }

        Ok(())
    }

    // ─── Persistence ───────────────────────────────────────────────

    /// Explicitly flush pending writes to disk.
    pub fn flush(&self) -> Result<()> {
        if let Some(v2) = &self.v2 {
            return v2.flush();
        }
        if self.is_in_memory() {
            return Ok(());
        }

        let mut handle = self.file_handle.lock();
        match handle.as_mut() {
            Some(file) => {
                file.flush()
                    .map_err(Error::io_err(&self.path, "flush"))?;
                file.sync_all()
                    .map_err(Error::io_err(&self.path, "fsync"))?;
            }
            // Nothing was appended yet: nothing to flush, and opening the
            // journal just to fsync it would create work where there is none.
            None if !self.dirty.load(Ordering::SeqCst) => return Ok(()),
            None => {
                drop(handle);
                fsync_journal(&self.path)?;
            }
        }
        self.dirty.store(false, Ordering::SeqCst);

        Ok(())
    }

    /// Whether this handle is backed by the v2 binary/mmap engine.
    pub fn is_v2(&self) -> bool {
        self.v2.is_some()
    }

    /// The live v2 manifest, when this is a v2 database.
    pub fn v2_manifest(&self) -> Option<V2Manifest> {
        self.v2.as_ref().map(|engine| engine.manifest())
    }

    /// Get the database file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    // ─── File Buckets ──────────────────────────────────────────────

    /// Get or create a named file bucket for binary storage.
    pub fn bucket(&self, name: &str) -> FileBucket {
        FileBucket::new(name, &self.base_dir)
    }

    /// Store a file under its real name, deriving the MIME type from the
    /// extension. The extension is preserved — `photo.png` stays a PNG.
    ///
    /// Serialized against `gc_buckets` by the writer lock (H3): an upload that
    /// lands mid-sweep can no longer be trashed before its document exists.
    pub fn store_file(&self, bucket: &str, data: &[u8], filename: &str) -> Result<FileMeta> {
        let _guard = self.writer.lock();
        let mime = bucket::mime_for_filename(filename);
        self.bucket(bucket).store(filename, data, mime)
    }

    /// List the stored files (`hash.ext`) of a bucket. An absent bucket is an
    /// empty list: nothing was ever stored there.
    pub fn list_files(&self, bucket: &str) -> Result<Vec<String>> {
        self.bucket(bucket).list()
    }

    // ─── Item buckets (#9) ──────────────────────────────────────────

    /// Access a declared item bucket. Fails unless the bucket is
    /// declared `kind: "items"` in `meta.json` — the declaration is
    /// what activates the kind, per the safety-by-construction rule.
    fn item_bucket(&self, name: &str) -> Result<&std::sync::Arc<ItemBucket>> {
        self.item_buckets.get(name).ok_or_else(|| {
            Error::invalid_arg(format!(
                "bucket '{name}' is not declared kind:\"items\" in meta.json"
            ))
        })
    }

    /// Names of all buckets declared `kind: "items"`.
    pub fn item_bucket_names(&self) -> Vec<String> {
        self.item_buckets.keys().cloned().collect()
    }

    /// Reserve an item: the engine creates `_files/<bucket>/<itemId>/`
    /// and returns the id and the folder path. The application streams
    /// bytes to that path itself — nDB is never in the ingest path.
    pub fn create_item(&self, bucket: &str) -> Result<(String, PathBuf)> {
        let _guard = self.writer.lock();
        self.item_bucket(bucket)?.create(self.persistence)
    }

    /// Commit a reserved item: one journal write carrying the caller's
    /// facts (`name`, `size`, `sha256` required; `mime` and anything
    /// else caller-supplied). The caller computes `sha256` while
    /// streaming — it touched every byte anyway.
    pub fn commit_item(&self, bucket: &str, item_id: &str, facts: Value) -> Result<()> {
        let _guard = self.writer.lock();
        self.item_bucket(bucket)?.commit(item_id, facts, self.persistence)
    }

    /// Read one item: record JSON plus the engine-issued `path`.
    pub fn read_item(&self, bucket: &str, item_id: &str) -> Result<Value> {
        let b = self.item_bucket(bucket)?;
        let record = b.get(item_id)?;
        Ok(Self::item_record_json(b, &record))
    }

    /// List items of a bucket. `state` filters to `"reserved"` or
    /// `"live"`; `None` lists both. Tombstoned items are never listed.
    pub fn list_items(&self, bucket: &str, state: Option<&str>) -> Result<Vec<Value>> {
        let filter = match state {
            None => None,
            Some("reserved") => Some(ItemState::Reserved),
            Some("live") => Some(ItemState::Live),
            Some(other) => {
                return Err(Error::invalid_arg(format!(
                    "item state filter must be \"reserved\" or \"live\", got '{other}'"
                )))
            }
        };
        let b = self.item_bucket(bucket)?;
        Ok(b.list(filter)
            .iter()
            .map(|r| Self::item_record_json(b, r))
            .collect())
    }

    fn item_record_json(b: &ItemBucket, record: &ItemRecord) -> Value {
        serde_json::json!({
            "itemId": record.id,
            "state": match record.state {
                ItemState::Reserved => "reserved",
                ItemState::Live => "live",
            },
            "facts": record.facts,
            "created": record.created,
            "committed": record.committed,
            "path": b.item_path(&record.id),
        })
    }

    /// Delete an item: record tombstoned, folder moved to trash.
    pub fn delete_item(&self, bucket: &str, item_id: &str) -> Result<()> {
        let _guard = self.writer.lock();
        self.item_bucket(bucket)?.delete(item_id, self.persistence)
    }

    /// Restore a tombstoned item: tombstone lifted, folder moved back.
    pub fn restore_item(&self, bucket: &str, item_id: &str) -> Result<()> {
        let _guard = self.writer.lock();
        self.item_bucket(bucket)?.restore(item_id, self.persistence)
    }

    /// Sweep reserved items older than the bucket's
    /// `reserved_ttl_seconds` (default 30 min): tombstoned and trashed,
    /// exactly like delete_item. Also runs in the TTL background thread.
    pub fn sweep_reserved_items(&self, bucket: &str) -> Result<usize> {
        let _guard = self.writer.lock();
        self.item_bucket(bucket)?.sweep_reserved(self.persistence)
    }

    /// Verify all item buckets: live originals match committed
    /// `size`/`sha256` (streaming — never read whole), reserved items
    /// have their folder, orphan folders are flagged sweepable. Returns
    /// human-readable anomalies, each prefixed with the bucket name.
    pub fn verify_item_buckets(&self) -> Vec<String> {
        let mut anomalies = Vec::new();
        for (name, bucket) in &self.item_buckets {
            for anomaly in bucket.verify() {
                anomalies.push(format!("{name}/{anomaly}"));
            }
        }
        anomalies
    }

    /// Targets a specific file reference string (e.g. `images:a1b2c3d4.png`)
    /// and safely removes it from the bucket if nothing still needs it.
    /// Returns true if it was safely deleted, false when it is referenced.
    ///
    /// "Referenced" means: held by a live document (in ANY accepted ref form —
    /// compact, `/api/buckets/…`, `/file/…`, `/files/…`, `/v2/dbs/…/files/…`)
    /// or by a restorable record in `_trash/docs/` (C4). A trashed document
    /// may be restored, and restoring it returns its files — deleting one of
    /// those would break the restore.
    pub fn release_file(&self, file_ref_str: &str) -> Result<bool> {
        self.release_file_with(file_ref_str, false)
    }

    /// Like [`Database::release_file`], but `force` deletes the file even when
    /// a live or restorable document still references it (`?force=true`).
    /// The caller is asserting it knows better; the dangling reference is then
    /// the caller's, not the engine's.
    pub fn release_file_with(&self, file_ref_str: &str, force: bool) -> Result<bool> {
        // G2: the scan of live documents and the trash of the file must
        // not interleave with a writer — an insert referencing this file
        // landing mid-scan would leave a live document pointing at a
        // trashed file. Serialized against all writers via the same lock
        // they hold.
        let _guard = self.writer.lock();

        let compact = Self::normalize_file_ref(file_ref_str)
            .ok_or_else(|| Error::invalid_arg(
                "Invalid file ref format, expected bucket:hash.ext, /api/buckets/…, /file/…, /files/… or /v2/dbs/<db>/files/…",
            ))?;
        let file_ref = FileRef::from_compact(&compact)
            .ok_or_else(|| Error::invalid_arg("Invalid file ref format, expected bucket:hash.ext"))?;

        if !force {
            // Live documents: normalize every string, so every accepted HTTP
            // form counts as a reference rather than only the exact spelling
            // stored.
            {
                let live = self.collect_live_file_refs()?;
                if live.contains(&compact) {
                    return Ok(false);
                }
            }

            // Restorable deletions: a document in the doc trash can come back,
            // and with it the files listed in its `_trashed_files`.
            if !self.is_in_memory() {
                let mut trashed = HashSet::new();
                for doc in storage::read_trash(&self.trash_doc_path())? {
                    Self::extract_file_refs(&doc, &mut trashed);
                }
                if trashed.contains(&compact) {
                    return Ok(false);
                }
            }
        }

        // Orphaned! Trash the file
        let bkt = self.bucket(&file_ref.bucket);
        bkt.delete(&file_ref)?;
        Ok(true)
    }

    /// Perform a full maintenance garbage collection of all buckets.
    /// Traverses all active documents, records `FileRef` patterns,
    /// then sweeps all buckets moving unreferenced physical files to trash.
    /// Returns the number of files moved to trash.
    pub fn gc_buckets(&self) -> Result<usize> {
        // G2: mark-and-sweep under the writer lock — without it, an insert
        // landing between the mark phase and the sweep could reference a
        // file the sweep is about to trash (dangling reference in a live
        // document). Maintenance sweeps blocking writers briefly is the
        // correct trade.
        let _guard = self.writer.lock();

        // 1. Mark phase: extract all possible `{bucket}:{hash}.{ext}` strings
        let active_refs = self.collect_live_file_refs()?;

        let mut trashed_count = 0;
        let files_base = self.base_dir.join("_files");
        
        if files_base.exists() {
            for entry in fs::read_dir(&files_base).map_err(Error::io_err(&files_base, "list base files"))? {
                let entry = entry.map_err(Error::io_err(&files_base, "read bucket entry"))?;
                let file_type = entry
                    .file_type()
                    .map_err(Error::io_err(entry.path(), "stat bucket entry"))?;
                if file_type.is_dir() {
                    let bucket_name = entry.file_name().to_string_lossy().to_string();
                    let bkt = self.bucket(&bucket_name);

                    // A bucket we cannot enumerate is named and skipped rather
                    // than raised: the caller asked for a sweep, and the count
                    // below still covers only files that really moved.
                    let files = match bkt.list() {
                        Ok(files) => files,
                        Err(e) => {
                            report::suppressed("gc_buckets: list bucket files", e);
                            continue;
                        }
                    };
                    for filename in files {
                        let ref_str = format!("{}:{}", bucket_name, filename);
                        if !active_refs.contains(&ref_str) {
                            // Missing from documents
                            if let Some(dot_pos) = filename.rfind('.') {
                                let id = filename[..dot_pos].to_string();
                                let ext = filename[dot_pos + 1..].to_string();
                                let file_ref = FileRef {
                                    bucket: bucket_name.clone(),
                                    id,
                                    ext,
                                };
                                // Count only what actually moved: the returned
                                // number is the caller's evidence that the
                                // sweep happened, so it must not include files
                                // that failed to move.
                                match bkt.delete(&file_ref) {
                                    Ok(()) => trashed_count += 1,
                                    Err(e) => report::suppressed(
                                        "gc_buckets: move unreferenced file to trash",
                                        e,
                                    ),
                                }
                            }
                        }
                    }
                }
            }
        }

        Ok(trashed_count)
    }

    /// Normalize any string that points at a bucket file into compact
    /// `bucket:hash.ext` form. Accepts every ref form an API may store:
    /// - compact nURI: `images:a1b2c3d4.png`
    /// - `/api/buckets/images/a1b2c3d4.png` (optional host prefix, query)
    /// - `/file/images/a1b2c3d4.png` (legacy single-DB route)
    /// - `/files/images/a1b2c3d4.png` (v2 route)
    /// - `/v2/dbs/<db>/files/images/a1b2c3d4.png`
    fn normalize_file_ref(s: &str) -> Option<String> {
        // Compact form first
        if let Some(fr) = FileRef::from_compact(s) {
            if fr.id.len() >= 8 && !fr.bucket.is_empty() && !fr.ext.is_empty() {
                // Reject URL-looking "buckets" mistaken as compact (no slashes in bucket)
                if !fr.bucket.contains('/') && !fr.id.contains('/') {
                    return Some(fr.to_string_compact());
                }
            }
        }

        // Path forms. `/files/` covers both the v2 route and the
        // `/v2/dbs/<db>/files/<bucket>/<file>` route (the tail is identical).
        for marker in ["/api/buckets/", "/files/", "/file/"] {
            if let Some(compact) = Self::compact_from_path_marker(s, marker) {
                return Some(compact);
            }
        }

        None
    }

    fn compact_from_path_marker(s: &str, marker: &str) -> Option<String> {
        let idx = s.find(marker)?;
        let rest = &s[idx + marker.len()..];
        let rest = rest.split(['?', '#']).next().unwrap_or(rest);
        let mut parts = rest.splitn(2, '/');
        let bucket = parts.next().unwrap_or("");
        let file = parts.next().unwrap_or("");
        if bucket.is_empty() || file.is_empty() || bucket.contains('/') {
            return None;
        }
        let file = file.split(['?', '#']).next().unwrap_or(file);
        let dot = file.rfind('.')?;
        let id = &file[..dot];
        let ext = &file[dot + 1..];
        if id.len() >= 8 && !ext.is_empty() {
            return Some(format!("{}:{}.{}", bucket, id, ext));
        }
        None
    }

    /// Every nURI file reference held by a live document, whichever backend
    /// owns the document store.
    fn collect_live_file_refs(&self) -> Result<HashSet<String>> {
        let mut refs = HashSet::new();
        if let Some(v2) = &self.v2 {
            v2.for_each_doc(|_, doc| Self::extract_file_refs(doc, &mut refs))?;
            return Ok(refs);
        }
        let docs = self.docs.read();
        for val in docs.values() {
            Self::extract_file_refs(val, &mut refs);
        }
        Ok(refs)
    }

    pub(crate) fn extract_file_refs(value: &Value, refs: &mut HashSet<String>) {
        match value {
            Value::String(s) => {
                if let Some(compact) = Self::normalize_file_ref(s) {
                    refs.insert(compact);
                }
            }
            Value::Array(a) => {
                for v in a {
                    Self::extract_file_refs(v, refs);
                }
            }
            Value::Object(o) => {
                for v in o.values() {
                    Self::extract_file_refs(v, refs);
                }
            }
            _ => {}
        }
    }

    /// Diff file refs between old and new state, update counters, and trash orphaned.
    fn handle_ref_delta_and_trash(&self, old_doc: &Value, new_doc: &Value) {
        let mut old_refs = HashSet::new();
        Self::extract_file_refs(old_doc, &mut old_refs);
        let mut new_refs = HashSet::new();
        Self::extract_file_refs(new_doc, &mut new_refs);

        let to_decrement: HashSet<_> = old_refs.difference(&new_refs).cloned().collect();
        let to_increment: HashSet<_> = new_refs.difference(&old_refs).cloned().collect();

        let mut orphaned_files = Vec::new();
        {
            let mut file_refs = self.file_refs.write();
            for r in to_decrement {
                if let Some(count) = file_refs.get_mut(&r) {
                    *count = count.saturating_sub(1);
                    if *count == 0 {
                        file_refs.remove(&r);
                        orphaned_files.push(r);
                    }
                }
            }
            for r in to_increment {
                *file_refs.entry(r).or_insert(0) += 1;
            }
        }

        for f in &orphaned_files {
            if let Some(file_ref) = FileRef::from_compact(f) {
                let bucket = self.bucket(&file_ref.bucket);
                // Automatic post-write cleanup: the document write has already
                // succeeded, so this reports instead of failing the operation.
                if let Err(e) = bucket.delete(&file_ref) {
                    report::suppressed("update: move orphaned file to trash", e);
                }
            }
        }
    }

    /// Increment file ref counts for all refs in a document
    fn increment_file_refs(&self, doc: &Value) {
        let mut extracted = HashSet::new();
        Self::extract_file_refs(doc, &mut extracted);
        if extracted.is_empty() { return; }
        
        let mut file_refs = self.file_refs.write();
        for r in extracted {
            *file_refs.entry(r).or_insert(0) += 1;
        }
    }
}

/// Fsync a journal file through its own handle. `FlushFileBuffers` flushes the
/// file's system buffers regardless of which handle asks, and on Windows needs
/// a write-capable handle — hence `write(true)` rather than `read(true)`.
fn fsync_journal(path: &Path) -> Result<()> {
    let file = fs::OpenOptions::new()
        .write(true)
        .open(path)
        .map_err(Error::io_err(path, "open journal for scheduled fsync"))?;
    file.sync_all()
        .map_err(Error::io_err(path, "scheduled fsync"))
}

/// Recursive directory copy for snapshot export (item folders, #9).
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {    fs::create_dir_all(dst).map_err(Error::io_err(dst, "create directory for copy"))?;
    for entry in fs::read_dir(src).map_err(Error::io_err(src, "read directory for copy"))? {
        let entry = entry.map_err(Error::io_err(src, "read directory entry for copy"))?;
        let target = dst.join(entry.file_name());
        if entry.file_type().map_or(false, |t| t.is_dir()) {
            copy_dir_recursive(&entry.path(), &target)?;
        } else {
            fs::copy(entry.path(), &target).map_err(Error::io_err(entry.path(), "copy file"))?;
        }
    }
    Ok(())
}

impl Error {
    fn index_error(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::IndexError {
            field: field.into(),
            reason: reason.into(),
        }
    }
}

impl Drop for Database {
    fn drop(&mut self) {
        // Stop the scheduled flusher first: its final pass must not race the
        // flush below (and the journal handle must stay valid while it runs).
        if let Some(tx) = self.flush_tx.lock().take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self.flush_thread.lock().take() {
            if handle.join().is_err() {
                report::detached("drop: join flush thread", "background thread panicked");
            }
        }

        // Signal the TTL background thread to stop. A send failure means the
        // thread already exited, which is the outcome we were after.
        if let Some(tx) = self.ttl_tx.lock().take() {
            let _ = tx.send(());
        }

        // Wait for it to finish gracefully to prevent Node.js hanging
        if let Some(handle) = self.ttl_thread.lock().take() {
            if handle.join().is_err() {
                report::detached("drop: join ttl thread", "background thread panicked");
            }
        }

        // Flush any pending writes. Drop has no error channel, and the process
        // is about to exit and take the unwritten data with it — the one
        // outcome that must never be silent.
        if let Err(e) = self.flush() {
            report::detached("drop: flush pending writes", e);
        }

        // `_lock` drops here: closing the handle is the OS lock release.
    }
}

// ─── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use tempfile::TempDir;

    fn test_db() -> (Database, TempDir) {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("test.jsonl");
        let db = Database::open(&path).unwrap();
        (db, dir)
    }

    // ─── File references (C4) ─────────────────────────────────────

    #[test]
    fn every_bucket_ref_form_normalizes_to_the_compact_uri() {
        let hash = "a".repeat(64);
        let expected = format!("images:{hash}.png");
        let forms = [
            expected.clone(),
            format!("/api/buckets/images/{hash}.png"),
            format!("https://host:3100/api/buckets/images/{hash}.png?w=10"),
            format!("/file/images/{hash}.png"),
            format!("/files/images/{hash}.png"),
            format!("/v2/dbs/photos/files/images/{hash}.png"),
        ];
        for form in forms {
            assert_eq!(
                Database::normalize_file_ref(&form).as_deref(),
                Some(expected.as_str()),
                "form {form} must be a live reference"
            );
        }
        assert_eq!(Database::normalize_file_ref("/files/images/"), None);
        assert_eq!(Database::normalize_file_ref("not a ref"), None);
    }

    #[test]
    fn a_doc_referencing_the_files_url_survives_gc_and_refuses_release() {
        let (db, _dir) = test_db();
        let meta = db.bucket("images").store("pic.png", b"bytes", "image/png").unwrap();
        let url = format!("/files/images/{}.png", meta._file.id);
        let id = db.insert(json!({"attachment": url})).unwrap();

        assert_eq!(db.gc_buckets().unwrap(), 0, "a URL-form ref is a live ref");
        assert!(db.bucket("images").exists(&meta._file));
        assert!(!db.release_file(&format!("/files/images/{}.png", meta._file.id)).unwrap());

        db.delete(&id).unwrap();
        // The restorable trash record still holds it.
        assert!(
            !db.release_file(&format!("images:{}.png", meta._file.id)).unwrap(),
            "a restorable document keeps its file"
        );
    }

    #[test]
    fn apply_ops_commits_once_and_rolls_back_entirely() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"a": 1, "list": [1], "tmp": true})).unwrap();
        let seq = db.doc_seq(&id).unwrap();

        let result = db
            .apply_ops(
                &id,
                &[
                    PatchOp::Set {
                        path: "a".to_string(),
                        value: json!(2),
                    },
                    PatchOp::ArrayPush {
                        path: "list".to_string(),
                        value: json!(2),
                    },
                    PatchOp::Remove {
                        path: "tmp".to_string(),
                    },
                ],
                Some(seq),
            )
            .unwrap();
        assert_eq!(result.applied, vec![true, true, true]);
        assert_eq!(result.seq, seq + 1, "one journal record for the batch");
        assert_eq!(result.doc, json!({"_id": id, "a": 2, "list": [1, 2]}));
        assert_eq!(db.lineage().last_seq, seq + 1);

        // A stale token conflicts; a bad path aborts without a record.
        let err = db
            .apply_ops(
                &id,
                &[PatchOp::Set {
                    path: "a".to_string(),
                    value: json!(3),
                }],
                Some(seq),
            )
            .unwrap_err();
        assert!(matches!(err, Error::Conflict { .. }));
        let err = db
            .apply_ops(
                &id,
                &[
                    PatchOp::Set {
                        path: "a".to_string(),
                        value: json!(3),
                    },
                    PatchOp::ArrayPush {
                        path: "a".to_string(),
                        value: json!(9),
                    },
                ],
                None,
            )
            .unwrap_err();
        assert!(matches!(err, Error::InvalidArgument { .. }));
        assert_eq!(db.get(&id).unwrap()["a"], json!(2));
        assert_eq!(db.lineage().last_seq, seq + 1, "nothing journaled");
    }

    // ─── Phase 1: Core CRUD ────────────────────────────────────────

    #[test]
    fn open_creates_file() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("new.jsonl");
        let db = Database::open(&path).unwrap();
        assert!(path.exists());
        assert_eq!(db.len(), 0);
    }

    #[test]
    fn open_loads_existing() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("existing.jsonl");
        let db = Database::open(&path).unwrap();
        let id = db.insert(json!({"name": "test"})).unwrap();
        drop(db);

        // Reopen
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.len(), 1);
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["name"], "test");
    }

    #[test]
    fn insert_returns_nanoid() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"title": "hello"})).unwrap();
        assert_eq!(id.len(), 16);
        assert!(id.chars().all(|c| c.is_ascii_alphanumeric()));
    }

    #[test]
    fn insert_with_prefix() {
        let (db, _dir) = test_db();
        let id = db.insert_with_prefix("conv", json!({"msg": "hi"})).unwrap();
        assert!(id.starts_with("conv_"));
        assert_eq!(id.len(), 21); // "conv_" + 16
    }

    #[test]
    fn get_by_id() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"val": 42})).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["val"], 42);
        assert_eq!(doc["_id"], id);
    }

    #[test]
    fn get_not_found() {
        let (db, _dir) = test_db();
        assert!(db.get("nonexistent").is_err());
    }

    #[test]
    fn delete_soft() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"x": 1})).unwrap();
        assert_eq!(db.len(), 1);

        db.delete(&id).unwrap();
        assert_eq!(db.len(), 0);
        assert!(db.get(&id).is_err());
        assert!(db.deleted_ids().contains(&id));
    }

    #[test]
    fn delete_not_found() {
        let (db, _dir) = test_db();
        assert!(db.delete("nonexistent").is_err());
    }

    #[test]
    fn in_memory_db() {
        let db = Database::open_in_memory().unwrap();
        let id = db.insert(json!({"a": 1})).unwrap();
        assert_eq!(db.len(), 1);
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["a"], 1);
    }

    // ─── Phase 2: Update, Iter, Compaction ─────────────────────────

    #[test]
    fn update_replaces_doc() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"v": 1})).unwrap();
        db.update(&id, json!({"v": 2})).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["v"], 2);
    }

    #[test]
    fn update_not_found() {
        let (db, _dir) = test_db();
        assert!(db.update("nonexistent", json!({"v": 1})).is_err());
    }

    #[test]
    fn iter_returns_all() {
        let (db, _dir) = test_db();
        db.insert(json!({"a": 1})).unwrap();
        db.insert(json!({"b": 2})).unwrap();
        db.insert(json!({"c": 3})).unwrap();
        let all = db.iter();
        assert_eq!(all.len(), 3);
    }

    #[test]
    fn iter_excludes_deleted() {
        let (db, _dir) = test_db();
        let id1 = db.insert(json!({"a": 1})).unwrap();
        db.insert(json!({"b": 2})).unwrap();
        db.delete(&id1).unwrap();
        let all = db.iter();
        assert_eq!(all.len(), 1);
        assert_eq!(all[0]["b"], 2);
    }

    #[test]
    fn compact_removes_tombstones() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("compact.jsonl");
        let db = Database::open(&path).unwrap();

        let id1 = db.insert(json!({"keep": true})).unwrap();
        let id2 = db.insert(json!({"delete": true})).unwrap();
        db.delete(&id2).unwrap();

        db.compact().unwrap();
        drop(db); // the OS writer lock forbids a second handle on one journal

        // Reopen and verify
        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.len(), 1);
        assert!(db2.get(&id1).is_ok());
        assert!(db2.get(&id2).is_err());
    }

    #[test]
    fn restore_deleted_doc() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("restore.jsonl");
        let db = Database::open(&path).unwrap();

        let id = db.insert(json!({"restore_me": true})).unwrap();
        db.delete(&id).unwrap();
        assert!(db.get(&id).is_err());

        db.restore(&id).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["restore_me"], true);
    }

    #[test]
    fn restore_refuses_a_live_document() {
        // G1: the trash file keeps entries after a restore, so without this
        // guard a second restore would re-inflate the ref counters (and a
        // restore of a never-deleted doc would resurrect a stale copy).
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("restore_guard.jsonl");
        let db = Database::open(&path).unwrap();

        let id = db.insert(json!({"v": 1})).unwrap();
        let err = db.restore(&id).unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { .. }),
            "restore of a live doc must be refused, got {err:?}"
        );

        db.delete(&id).unwrap();
        db.restore(&id).unwrap();
        let err = db.restore(&id).unwrap_err();
        assert!(
            matches!(err, Error::InvalidArgument { .. }),
            "double restore must be refused, got {err:?}"
        );
        // The refused double-restore left the document exactly as it was.
        assert_eq!(db.get(&id).unwrap()["v"], json!(1));
    }

    #[test]
    fn persistence_immediate() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("immediate.jsonl");
        let db = Database::open(&path)
            .unwrap()
            .with_persistence(Persistence::Immediate);

        let id = db.insert(json!({"safe": true})).unwrap();
        drop(db);

        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.len(), 1);
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["safe"], true);
    }

    // ─── Phase 4: Query Layer ──────────────────────────────────────

    #[test]
    fn find_equality() {
        let (db, _dir) = test_db();
        db.insert(json!({"name": "alice", "age": 30})).unwrap();
        db.insert(json!({"name": "bob", "age": 25})).unwrap();
        db.insert(json!({"name": "alice", "age": 35})).unwrap();

        let results = db.find("name", &json!("alice"));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn find_with_index() {
        let (db, _dir) = test_db();
        db.insert(json!({"email": "a@test.com"})).unwrap();
        db.insert(json!({"email": "b@test.com"})).unwrap();
        db.create_index("email").unwrap();

        let results = db.find("email", &json!("a@test.com"));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["email"], "a@test.com");
    }

    #[test]
    fn find_where_predicate() {
        let (db, _dir) = test_db();
        db.insert(json!({"score": 50})).unwrap();
        db.insert(json!({"score": 150})).unwrap();
        db.insert(json!({"score": 200})).unwrap();

        let results = db.find_where("score", |v| v.as_i64().unwrap_or(0) > 100);
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn find_range() {
        let (db, _dir) = test_db();
        db.insert(json!({"val": 10})).unwrap();
        db.insert(json!({"val": 50})).unwrap();
        db.insert(json!({"val": 100})).unwrap();

        let results = db.find_range("val", &json!(20), &json!(80));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["val"], 50);
    }

    #[test]
    fn query_simple_eq() {
        let (db, _dir) = test_db();
        db.insert(json!({"status": "active", "name": "a"})).unwrap();
        db.insert(json!({"status": "deleted", "name": "b"})).unwrap();

        let results = db.query(json!({"status": {"$eq": "active"}})).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], "a");
    }

    #[test]
    fn query_implicit_eq() {
        let (db, _dir) = test_db();
        db.insert(json!({"color": "red"})).unwrap();
        db.insert(json!({"color": "blue"})).unwrap();

        let results = db.query(json!({"color": "red"})).unwrap();
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_comparison_ops() {
        let (db, _dir) = test_db();
        db.insert(json!({"score": 10})).unwrap();
        db.insert(json!({"score": 50})).unwrap();
        db.insert(json!({"score": 100})).unwrap();

        let gt = db.query(json!({"score": {"$gt": 40}})).unwrap();
        assert_eq!(gt.len(), 2);

        let lt = db.query(json!({"score": {"$lt": 60}})).unwrap();
        assert_eq!(lt.len(), 2);

        let gte = db.query(json!({"score": {"$gte": 50}})).unwrap();
        assert_eq!(gte.len(), 2);

        let lte = db.query(json!({"score": {"$lte": 50}})).unwrap();
        assert_eq!(lte.len(), 2);
    }

    #[test]
    fn query_ne() {
        let (db, _dir) = test_db();
        db.insert(json!({"x": 1})).unwrap();
        db.insert(json!({"x": 2})).unwrap();

        let results = db.query(json!({"x": {"$ne": 1}})).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["x"], 2);
    }

    #[test]
    fn query_in() {
        let (db, _dir) = test_db();
        db.insert(json!({"status": "active"})).unwrap();
        db.insert(json!({"status": "pending"})).unwrap();
        db.insert(json!({"status": "deleted"})).unwrap();

        let results = db.query(json!({"status": {"$in": ["active", "pending"]}})).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_nin() {
        let (db, _dir) = test_db();
        db.insert(json!({"status": "active"})).unwrap();
        db.insert(json!({"status": "pending"})).unwrap();
        db.insert(json!({"status": "deleted"})).unwrap();

        let results = db.query(json!({"status": {"$nin": ["deleted"]}})).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_exists() {
        let (db, _dir) = test_db();
        db.insert(json!({"name": "a", "avatar": "yes"})).unwrap();
        db.insert(json!({"name": "b"})).unwrap();

        let exists = db.query(json!({"avatar": {"$exists": true}})).unwrap();
        assert_eq!(exists.len(), 1);

        let not_exists = db.query(json!({"avatar": {"$exists": false}})).unwrap();
        assert_eq!(not_exists.len(), 1);
        assert_eq!(not_exists[0]["name"], "b");
    }

    #[test]
    fn query_and_combinator() {
        let (db, _dir) = test_db();
        db.insert(json!({"user": "alice", "status": "active", "score": 150})).unwrap();
        db.insert(json!({"user": "alice", "status": "deleted", "score": 50})).unwrap();
        db.insert(json!({"user": "bob", "status": "active", "score": 200})).unwrap();

        let results = db.query(json!({
            "$and": [
                {"user": {"$eq": "alice"}},
                {"status": {"$eq": "active"}}
            ]
        })).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["score"], 150);
    }

    #[test]
    fn query_or_combinator() {
        let (db, _dir) = test_db();
        db.insert(json!({"x": 1})).unwrap();
        db.insert(json!({"x": 2})).unwrap();
        db.insert(json!({"x": 3})).unwrap();

        let results = db.query(json!({
            "$or": [
                {"x": {"$eq": 1}},
                {"x": {"$eq": 3}}
            ]
        })).unwrap();
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_not_combinator() {
        let (db, _dir) = test_db();
        db.insert(json!({"status": "active"})).unwrap();
        db.insert(json!({"status": "deleted"})).unwrap();

        let results = db.query(json!({
            "$not": {"status": {"$eq": "deleted"}}
        })).unwrap();
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["status"], "active");
    }

    #[test]
    fn query_with_limit_sort_offset() {
        let (db, _dir) = test_db();
        for i in 0..10 {
            db.insert(json!({"score": i * 10})).unwrap();
        }

        let results = db.query_with(
            json!({"score": {"$gte": 0}}),
            QueryOptions {
                limit: Some(3),
                offset: Some(2),
                sort_by: Some(("score".to_string(), SortDir::Desc)),
            },
        ).unwrap();
        assert_eq!(results.len(), 3);
        // Descending: 90, 80, 70, 60, 50, 40, 30, 20, 10, 0
        // Offset 2: 70, 60, 50
        assert_eq!(results[0]["score"], 70);
        assert_eq!(results[1]["score"], 60);
        assert_eq!(results[2]["score"], 50);
    }

    // ─── Index Management ──────────────────────────────────────────

    #[test]
    fn create_and_drop_index() {
        let (db, _dir) = test_db();
        db.insert(json!({"email": "test@test.com"})).unwrap();
        db.create_index("email").unwrap();
        assert!(db.has_index("email"));
        db.drop_index("email").unwrap();
        assert!(!db.has_index("email"));
    }

    #[test]
    fn drop_nonexistent_index() {
        let (db, _dir) = test_db();
        assert!(db.drop_index("nope").is_err());
    }

    #[test]
    fn index_updates_on_insert() {
        let (db, _dir) = test_db();
        db.create_index("name").unwrap();
        db.insert(json!({"name": "alice"})).unwrap();
        db.insert(json!({"name": "bob"})).unwrap();

        let results = db.find("name", &json!("alice"));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn index_updates_on_delete() {
        let (db, _dir) = test_db();
        db.create_index("name").unwrap();
        let id = db.insert(json!({"name": "alice"})).unwrap();
        db.delete(&id).unwrap();

        let results = db.find("name", &json!("alice"));
        assert_eq!(results.len(), 0);
    }

    #[test]
    fn index_updates_on_update() {
        let (db, _dir) = test_db();
        db.create_index("name").unwrap();
        let id = db.insert(json!({"name": "alice"})).unwrap();
        db.update(&id, json!({"name": "bob"})).unwrap();

        let alice = db.find("name", &json!("alice"));
        let bob = db.find("name", &json!("bob"));
        assert_eq!(alice.len(), 0);
        assert_eq!(bob.len(), 1);
    }

    // ─── Flush ─────────────────────────────────────────────────────

    #[test]
    fn flush_persists_data() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("flush.jsonl");
        let db = Database::open(&path).unwrap();

        db.insert(json!({"flush": true})).unwrap();
        db.flush().unwrap();
        drop(db);

        let db2 = Database::open(&path).unwrap();
        assert_eq!(db2.len(), 1);
    }

    // ─── Edge Cases ────────────────────────────────────────────────

    #[test]
    fn empty_db_operations() {
        let (db, _dir) = test_db();
        assert!(db.is_empty());
        assert_eq!(db.len(), 0);
        assert_eq!(db.iter().len(), 0);
        assert_eq!(db.find("x", &json!(1)).len(), 0);
        assert_eq!(db.query(json!({"x": 1})).unwrap().len(), 0);
    }

    #[test]
    fn contains_check() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"x": 1})).unwrap();
        assert!(db.contains(&id));
        assert!(!db.contains("nonexistent"));
    }

    #[test]
    fn multiple_inserts_unique_ids() {
        let (db, _dir) = test_db();
        let mut ids = HashSet::new();
        for i in 0..100 {
            let id = db.insert(json!({"i": i})).unwrap();
            assert!(ids.insert(id), "duplicate ID generated");
        }
        assert_eq!(db.len(), 100);
    }

    // ─── Atomic set Operations ─────────────────────────────────────

    #[test]
    fn set_top_level_field() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"title": "old", "count": 0})).unwrap();
        db.set(&id, "title", json!("new")).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["title"], "new");
        assert_eq!(doc["count"], 0);
    }

    #[test]
    fn set_nested_field() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({
            "settings": {"theme": "light", "lang": "en"}
        })).unwrap();
        db.set(&id, "settings.theme", json!("dark")).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["settings"]["theme"], "dark");
        assert_eq!(doc["settings"]["lang"], "en");
    }

    #[test]
    fn set_deeply_nested() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({
            "a": {"b": {"c": {"d": "old"}}}
        })).unwrap();
        db.set(&id, "a.b.c.d", json!("new")).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["a"]["b"]["c"]["d"], "new");
    }

    #[test]
    fn set_array_element_by_index() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({
            "messages": [
                {"text": "hello", "author": "alice"},
                {"text": "world", "author": "bob"},
                {"text": "foo", "author": "carol"}
            ]
        })).unwrap();
        db.set(&id, "messages.1.text", json!("earth")).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["messages"][1]["text"], "earth");
        assert_eq!(doc["messages"][1]["author"], "bob");
        assert_eq!(doc["messages"][0]["text"], "hello");
        assert_eq!(doc["messages"][2]["text"], "foo");
    }

    #[test]
    fn set_creates_new_field() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"existing": true})).unwrap();
        db.set(&id, "new_field", json!(42)).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["existing"], true);
        assert_eq!(doc["new_field"], 42);
    }

    #[test]
    fn set_nonexistent_doc_errors() {
        let (db, _dir) = test_db();
        let result = db.set("ghost", "field", json!("val"));
        assert!(result.is_err());
    }

    #[test]
    fn set_out_of_bounds_index_noop() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"items": [1, 2, 3]})).unwrap();
        db.set(&id, "items.10", json!(99)).unwrap();
        let doc = db.get(&id).unwrap();
        let arr = doc["items"].as_array().unwrap();
        assert_eq!(arr.len(), 3);
        assert_eq!(arr[0], 1);
        assert_eq!(arr[1], 2);
        assert_eq!(arr[2], 3);
    }

    #[test]
    fn set_nonexistent_path_segment_noop() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"x": 1})).unwrap();
        db.set(&id, "nonexistent.deep.path", json!("val")).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["x"], 1);
        assert!(doc.get("nonexistent").is_none());
    }

    #[test]
    fn set_type_variety() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"s": "", "n": 0, "b": false, "a": [], "o": {}})).unwrap();
        db.set(&id, "s", json!("string")).unwrap();
        db.set(&id, "n", json!(3.14)).unwrap();
        db.set(&id, "b", json!(true)).unwrap();
        db.set(&id, "a", json!([1, 2, 3])).unwrap();
        db.set(&id, "o", json!({"key": "val"})).unwrap();
        db.set(&id, "null_val", json!(null)).unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["s"], "string");
        assert_eq!(doc["n"], 3.14);
        assert_eq!(doc["b"], true);
        assert_eq!(doc["a"], json!([1, 2, 3]));
        assert_eq!(doc["o"], json!({"key": "val"}));
        assert!(doc["null_val"].is_null());
    }

    // ─── Atomic remove Operations ───────────────────────────────────

    #[test]
    fn remove_top_level_field() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"keep": true, "drop": "me"})).unwrap();
        db.remove(&id, "drop").unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["keep"], true);
        assert!(doc.get("drop").is_none());
    }

    #[test]
    fn remove_nested_field() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({
            "settings": {"theme": "dark", "lang": "en", "volume": 80}
        })).unwrap();
        db.remove(&id, "settings.volume").unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["settings"]["theme"], "dark");
        assert_eq!(doc["settings"]["lang"], "en");
        assert!(doc["settings"].get("volume").is_none());
    }

    #[test]
    fn remove_array_element_shifts() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"items": [10, 20, 30, 40]})).unwrap();
        db.remove(&id, "items.1").unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["items"], json!([10, 30, 40]));
    }

    #[test]
    fn remove_array_first_and_last() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"items": [1, 2, 3]})).unwrap();
        db.remove(&id, "items.0").unwrap();
        assert_eq!(db.get(&id).unwrap()["items"], json!([2, 3]));
        db.remove(&id, "items.1").unwrap();
        assert_eq!(db.get(&id).unwrap()["items"], json!([2]));
    }

    #[test]
    fn remove_out_of_bounds_noop() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"items": [1, 2]})).unwrap();
        db.remove(&id, "items.5").unwrap();
        assert_eq!(db.get(&id).unwrap()["items"], json!([1, 2]));
    }

    #[test]
    fn remove_nonexistent_field_noop() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"x": 1})).unwrap();
        db.remove(&id, "nonexistent").unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["x"], 1);
        assert!(doc.get("nonexistent").is_none());
    }

    #[test]
    fn remove_nonexistent_doc_errors() {
        let (db, _dir) = test_db();
        let result = db.remove("ghost", "field");
        assert!(result.is_err());
    }

    #[test]
    fn remove_nested_array_element() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({
            "conversations": [
                {"messages": ["a", "b", "c"]},
                {"messages": ["d", "e"]}
            ]
        })).unwrap();
        db.remove(&id, "conversations.0.messages.1").unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["conversations"][0]["messages"], json!(["a", "c"]));
        assert_eq!(doc["conversations"][1]["messages"], json!(["d", "e"]));
    }

    // ─── Persistence & Replay (set + remove) ─────────────────────────

    #[test]
    fn set_persists_and_replays() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("set_replay.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({"title": "old", "score": 0})).unwrap();
            db.set(&id, "title", json!("new")).unwrap();
            db.set(&id, "score", json!(100)).unwrap();
            db.flush().unwrap();
            id
        };

        let db2 = Database::open(&path).unwrap();
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["title"], "new");
        assert_eq!(doc["score"], 100);
    }

    #[test]
    fn remove_persists_and_replays() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("remove_replay.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({"a": 1, "b": 2, "c": 3})).unwrap();
            db.remove(&id, "b").unwrap();
            db.flush().unwrap();
            id
        };

        let db2 = Database::open(&path).unwrap();
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["a"], 1);
        assert_eq!(doc["c"], 3);
        assert!(doc.get("b").is_none());
    }

    // ─── Applied Flags (D1) ────────────────────────────────────────

    #[test]
    fn set_returns_applied_flag() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"title": "old", "items": [1, 2]})).unwrap();
        assert!(db.set(&id, "title", json!("new")).unwrap(), "resolved path applies");
        assert!(
            db.set(&id, "title", json!("new")).unwrap(),
            "same-value re-set is an applied write, not a no-op"
        );
        assert!(db.set(&id, "new_field", json!(1)).unwrap(), "leaf creation applies");
        assert!(!db.set(&id, "no.such.path", json!(1)).unwrap(), "missing intermediate is a no-op");
        assert!(!db.set(&id, "items.9", json!(1)).unwrap(), "out-of-bounds index is a no-op");
    }

    #[test]
    fn remove_returns_applied_flag() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"a": 1})).unwrap();
        assert!(db.remove(&id, "a").unwrap());
        assert!(!db.remove(&id, "a").unwrap(), "removing a missing key is a no-op");
        assert!(!db.remove(&id, "no.such.path").unwrap());
    }

    #[test]
    fn array_push_returns_applied_flag() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"threads": [{"msgs": ["a"]}]})).unwrap();
        assert!(db.array_push(&id, "tags", json!("a")).unwrap(), "array creation applies");
        assert!(db.array_push(&id, "tags", json!("b")).unwrap());
        assert!(
            db.array_push(&id, "threads.0.msgs", json!("b")).unwrap(),
            "push into a nested array applies"
        );
        assert!(!db.array_push(&id, "threads.9.msgs", json!("x")).unwrap(), "out-of-bounds index is a no-op");
        assert!(!db.array_push(&id, "tags.0", json!(2)).unwrap(), "push onto a scalar element is a no-op");
    }

    #[test]
    fn no_op_writes_no_journal_line() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("noop_journal.jsonl");

        let id = {
            let db = Database::open(&path)
                .unwrap()
                .with_persistence(Persistence::Immediate);
            let id = db.insert(json!({"title": "t"})).unwrap();
            assert!(!db.set(&id, "no.such.path", json!(1)).unwrap());
            assert!(!db.remove(&id, "ghost.field").unwrap());
            assert!(!db.array_push(&id, "items.9", json!(1)).unwrap());
            id
        };

        let journal = std::fs::read_to_string(&path).unwrap();
        let op_lines: Vec<&str> = journal.lines().filter(|l| l.contains("\"_op\"")).collect();
        assert_eq!(
            op_lines.len(),
            0,
            "no-op ops must not touch the journal, got {}: {}",
            op_lines.len(),
            journal
        );
        // The insert itself did land: exactly one non-meta line.
        let doc_lines: Vec<&str> = journal.lines().filter(|l| !l.contains("_meta")).collect();
        assert_eq!(doc_lines.len(), 1, "the insert must be journaled: {}", journal);
        assert!(doc_lines[0].contains(&id));
    }

    // ─── Trash Purge Race (B3) ────────────────────────────────────

    #[test]
    fn purge_during_deletes_never_loses_a_fresh_trash_record() {
        // A delete that returned Ok promised a restorable record. With a 1h
        // TTL every record written here is fresh, so the only way one can
        // vanish is the purgers' read-all→rewrite racing the append. Three
        // racers: the deleter, a foreground purger, and the TTL thread
        // (5 ms interval). Afterwards every delete must still restore.
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("trash_race.jsonl");
        let db = std::sync::Arc::new(
            Database::open(&path)
                .unwrap()
                .with_trash_mode(TrashMode::TTL(Duration::from_secs(3600)))
                .with_trash_ttl(Duration::from_secs(3600), Duration::from_millis(5)),
        );

        const N: usize = 300;
        let ids: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));

        // The trash file's first creation is a check-then-init race
        // (append_doc_trash); the trash lock serializes every real
        // participant, but the test's unlocked seed does not take it.
        // Create the file synchronously first, so the racers only ever
        // append. This delete is fresh too — it joins the check below.
        let first = db.insert(json!({"i": -1})).unwrap();
        db.delete(&first).unwrap();
        ids.lock().push(first);

        let deleter = {
            let db = std::sync::Arc::clone(&db);
            let ids = std::sync::Arc::clone(&ids);
            std::thread::spawn(move || {
                for i in 0..N {
                    let id = db.insert(json!({"i": i})).unwrap();
                    db.delete(&id).unwrap();
                    ids.lock().push(id);
                }
            })
        };
        let purger = {
            let db = std::sync::Arc::clone(&db);
            let ids = std::sync::Arc::clone(&ids);
            std::thread::spawn(move || {
                // A purge only rewrites the trash file when something
                // actually expired — with fresh-only trash the racy
                // read→rewrite window never opens. Seed a disposable
                // ancient record before every sweep so each purge does
                // real rewrite work while the deleter appends.
                let seed = json!({"_id": "purge_churn_seed", "_deleted": 1u64});
                while ids.lock().len() < N {
                    // Seed under the trash lock like every real writer: a
                    // trash line is written as multiple syscalls, so an
                    // UNSERIALIZED appender could interleave mid-line with
                    // delete's append and corrupt it — line tearing, not
                    // the B3 race this test hunts.
                    let seeded = {
                        let _g = db.trash_lock.lock();
                        storage::append_doc_trash(&db.trash_doc_path(), &seed)
                    };
                    if seeded.is_err() {
                        continue;
                    }
                    db.purge_trash().unwrap();
                }
            })
        };
        deleter.join().unwrap();
        purger.join().unwrap();

        let lost = ids.lock().len();
        assert_eq!(lost, N + 1, "deleter must finish all deletes (plus the pre-seed)");
        for id in ids.lock().iter() {
            db.restore(id)
                .unwrap_or_else(|e| panic!("fresh trash record for {id} lost: {e}"));
        }
    }

    // ─── Concurrency of the maintenance locks (G2) ────────────
    //
    // Why this is not a race-reproduction test: gc's mark phase holds the
    // docs read lock for its whole scan (consistent snapshot), and the
    // end-state of the illegal interleave ("doc references a trashed
    // file") is indistinguishable from the legal ordering ("gc completed,
    // then the doc referenced an already-trashed file"). The writer lock
    // makes sweeps serializable against writers — correct by construction,
    // not provable by a black-box hammer. What CAN be falsified: the
    // combined lock graph (writer, trash_lock, gc sweep, purge, delta
    // writers) deadlocking or corrupting under concurrent load.

    #[test]
    fn gc_purge_and_writers_run_concurrently_without_corruption() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("gc_locks.jsonl");
        let db = std::sync::Arc::new(
            Database::open(&path)
                .unwrap()
                .with_persistence(Persistence::Immediate),
        );
        let bucket = db.bucket("images");

        // Widen the sweeper's mark phase.
        for i in 0..200 {
            db.insert(json!({"filler": i, "blob": "x".repeat(50)})).unwrap();
        }

        // Anchor docs: each file stays referenced for the whole test, so
        // hammer-doc deletion (refcount still ≥ 1 via the anchor) can
        // never orphan the file — the only files that vanish here would
        // be trashed illegally.
        let anchors: Vec<String> = (0..10)
            .map(|i| {
                let meta = bucket
                    .store(&format!("g{i}.png"), format!("anchor-{i}").as_bytes(), "image/png")
                    .unwrap();
                let r = meta._file.to_string_compact();
                db.insert(json!({"anchor": true, "img": r})).unwrap();
                r
            })
            .collect();

        let stop = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut loop_handles = Vec::new();

        // Sweeper and purger loop until stopped. A deadlock in the lock
        // graph shows up as this test hanging on join.
        {
            let db = std::sync::Arc::clone(&db);
            let stop = std::sync::Arc::clone(&stop);
            loop_handles.push(std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    db.gc_buckets().unwrap();
                }
            }));
        }
        {
            let db = std::sync::Arc::clone(&db);
            let stop = std::sync::Arc::clone(&stop);
            loop_handles.push(std::thread::spawn(move || {
                while !stop.load(std::sync::atomic::Ordering::Relaxed) {
                    db.purge_trash().unwrap();
                }
            }));
        }

        // Writer hammer: a bounded number of inserts + delta ops + deletes,
        // referencing the anchored files round-robin. Joined FIRST — the
        // loop threads above are stopped only after the hammer finishes.
        const HAMMER: usize = 300;
        let ids: std::sync::Arc<Mutex<Vec<String>>> =
            std::sync::Arc::new(Mutex::new(Vec::new()));
        let mut hammer_handles = Vec::new();
        {
            let db = std::sync::Arc::clone(&db);
            let ids = std::sync::Arc::clone(&ids);
            let anchors = anchors.clone();
            hammer_handles.push(std::thread::spawn(move || {
                for n in 0..HAMMER {
                    let id = db
                        .insert(json!({"hammer": n, "img": anchors[n % anchors.len()]}))
                        .unwrap();
                    db.set(&id, "hammer", json!(n + 1)).unwrap();
                    ids.lock().push(id);
                }
            }));
        }
        {
            let db = std::sync::Arc::clone(&db);
            let ids = std::sync::Arc::clone(&ids);
            hammer_handles.push(std::thread::spawn(move || {
                loop {
                    let popped = ids.lock().pop();
                    match popped {
                        Some(id) => {
                            db.delete(&id).unwrap();
                        }
                        None => break,
                    }
                }
            }));
        }

        for h in hammer_handles {
            h.join().unwrap();
        }
        stop.store(true, std::sync::atomic::Ordering::Relaxed);
        for h in loop_handles {
            h.join().unwrap();
        }

        // Anchored files survived every sweep.
        for r in &anchors {
            let fr = FileRef::from_compact(r).unwrap();
            assert!(
                db.bucket(&fr.bucket).exists(&fr),
                "anchored (permanently referenced) file {r} was trashed — illegal sweep"
            );
        }

        // Every live document's references resolve.
        for doc in db.iter() {
            let mut refs = HashSet::new();
            Database::extract_file_refs(&doc, &mut refs);
            for r in refs {
                let fr = FileRef::from_compact(&r)
                    .unwrap_or_else(|| panic!("unparseable ref {r} in {doc}"));
                assert!(
                    db.bucket(&fr.bucket).exists(&fr),
                    "dangling reference {r} in live doc — corruption under concurrency"
                );
            }
        }
    }

    #[test]
    fn mixed_ops_persist_and_replay() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("mixed_replay.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({
                "messages": [{"text": "hi", "author": "alice"}],
                "title": "Chat",
                "count": 1
            })).unwrap();
            db.array_push(&id, "messages", json!({"text": "there", "author": "bob"})).unwrap();
            db.set(&id, "title", json!("Updated Chat")).unwrap();
            db.set(&id, "count", json!(2)).unwrap();
            db.set(&id, "messages.0.text", json!("hello")).unwrap();
            db.remove(&id, "messages.1.author").unwrap();
            db.flush().unwrap();
            id
        };

        let db2 = Database::open(&path).unwrap();
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["title"], "Updated Chat");
        assert_eq!(doc["count"], 2);
        assert_eq!(doc["messages"][0]["text"], "hello");
        assert_eq!(doc["messages"][0]["author"], "alice");
        assert_eq!(doc["messages"][1]["text"], "there");
        assert!(doc["messages"][1].get("author").is_none());
    }

    #[test]
    fn full_update_overwrites_patches() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("overwrite.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({"x": 1, "y": 2})).unwrap();
            db.set(&id, "x", json!(99)).unwrap();
            db.update(&id, json!({"_id": id, "z": 3})).unwrap();
            db.set(&id, "z", json!(42)).unwrap();
            db.flush().unwrap();
            id
        };

        let db2 = Database::open(&path).unwrap();
        let doc = db2.get(&id).unwrap();
        assert!(doc.get("x").is_none());
        assert!(doc.get("y").is_none());
        assert_eq!(doc["z"], 42);
    }

    #[test]
    fn patches_after_tombstone_noop() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("tombstone_patch.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({"val": 1})).unwrap();
            db.delete(&id).unwrap();
            assert!(db.set(&id, "val", json!(99)).is_err());
            db.flush().unwrap();
            id
        };

        let db2 = Database::open(&path).unwrap();
        assert!(db2.get(&id).is_err());
        assert_eq!(db2.len(), 0);
    }

    #[test]
    fn compact_bakes_set_patches() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("compact_set.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({"x": 0})).unwrap();
            for i in 1..=50 {
                db.set(&id, "x", json!(i)).unwrap();
            }
            db.compact().unwrap();
            id
        };

        let file_content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = file_content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "compact should produce meta header + 1 doc");

        let db2 = Database::open(&path).unwrap();
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["x"], 50);
    }

    #[test]
    fn compact_bakes_remove_patches() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("compact_remove.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({"a": 1, "b": 2, "c": 3, "d": 4})).unwrap();
            db.remove(&id, "b").unwrap();
            db.remove(&id, "d").unwrap();
            db.compact().unwrap();
            id
        };

        let file_content = std::fs::read_to_string(&path).unwrap();
        let lines: Vec<&str> = file_content.lines().filter(|l| !l.is_empty()).collect();
        assert_eq!(lines.len(), 2, "compact should produce meta header + 1 doc");

        let db2 = Database::open(&path).unwrap();
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["a"], 1);
        assert_eq!(doc["c"], 3);
        assert!(doc.get("b").is_none());
        assert!(doc.get("d").is_none());
    }

    // ─── Stress Tests: set + remove ──────────────────────────────────

    #[test]
    fn stress_rapid_set_same_path() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"counter": 0})).unwrap();
        for i in 1..=500 {
            db.set(&id, "counter", json!(i)).unwrap();
        }
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["counter"], 500);
    }

    #[test]
    fn stress_rapid_set_multiple_paths() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({
            "a": 0, "b": 0, "c": 0, "d": 0, "e": 0
        })).unwrap();
        for i in 1..=200 {
            db.set(&id, "a", json!(i)).unwrap();
            db.set(&id, "b", json!(i * 2)).unwrap();
            db.set(&id, "c", json!(i * 3)).unwrap();
            db.set(&id, "d", json!(format!("val_{}", i))).unwrap();
            db.set(&id, "e", json!(null)).unwrap();
        }
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["a"], 200);
        assert_eq!(doc["b"], 400);
        assert_eq!(doc["c"], 600);
        assert_eq!(doc["d"], "val_200");
        assert_eq!(doc["e"], Value::Null);
    }

    #[test]
    fn stress_set_nested_array_elements() {
        let (db, _dir) = test_db();
        let items: Vec<Value> = (0..100).map(|i| json!({"v": i, "label": format!("item_{}", i)})).collect();
        let id = db.insert(json!({"items": items.clone()})).unwrap();
        for i in 0..100 {
            db.set(&id, &format!("items.{}.v", i), json!(i * 10)).unwrap();
            db.set(&id, &format!("items.{}.label", i), json!(format!("updated_{}", i))).unwrap();
        }
        let doc = db.get(&id).unwrap();
        for i in 0..100 {
            assert_eq!(doc["items"][i]["v"], i * 10);
            assert_eq!(doc["items"][i]["label"], format!("updated_{}", i));
        }
    }

    #[test]
    fn stress_alternating_set_and_remove() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({
            "keep": "forever",
            "temp1": "a",
            "temp2": "b",
            "temp3": "c"
        })).unwrap();
        for i in 0..200 {
            let field = format!("temp{}", (i % 3) + 1);
            if i % 2 == 0 {
                db.set(&id, &field, json!(format!("set_{}", i))).unwrap();
            } else {
                db.remove(&id, &field).unwrap();
            }
        }
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["keep"], "forever");
    }

    #[test]
    fn stress_array_push_then_set_elements() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"messages": []})).unwrap();
        for i in 0..200 {
            db.array_push(&id, "messages", json!({"text": format!("msg_{}", i), "edited": false})).unwrap();
        }
        for i in 0..200 {
            if i % 5 == 0 {
                db.set(&id, &format!("messages.{}.edited", i), json!(true)).unwrap();
                db.set(&id, &format!("messages.{}.text", i), json!(format!("edited_{}", i))).unwrap();
            }
        }
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["messages"].as_array().unwrap().len(), 200);
        assert_eq!(doc["messages"][0]["text"], "edited_0");
        assert_eq!(doc["messages"][0]["edited"], true);
        assert_eq!(doc["messages"][1]["text"], "msg_1");
        assert_eq!(doc["messages"][1]["edited"], false);
        assert_eq!(doc["messages"][5]["text"], "edited_5");
    }

    #[test]
    fn stress_persist_replay_large_set_chain() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stress_replay.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({
                "counter": 0,
                "values": [],
                "label": "init"
            })).unwrap();
            for i in 1..=100 {
                db.set(&id, "counter", json!(i)).unwrap();
                db.set(&id, "label", json!(format!("step_{}", i))).unwrap();
                db.array_push(&id, "values", json!(i)).unwrap();
                if i % 10 == 0 {
                    let idx = i - 10;
                    db.set(&id, &format!("values.{}", idx), json!(i * 100)).unwrap();
                }
            }
            db.flush().unwrap();
            id
        };

        let db2 = Database::open(&path).unwrap();
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["counter"], 100);
        assert_eq!(doc["label"], "step_100");
        assert_eq!(doc["values"].as_array().unwrap().len(), 100);
        assert_eq!(doc["values"][0], 1000);
        assert_eq!(doc["values"][10], 2000);
        assert_eq!(doc["values"][99], 100);
    }

    #[test]
    fn stress_persist_replay_then_compact() {
        let dir = TempDir::new().unwrap();
        let path = dir.path().join("stress_compact.jsonl");

        let id = {
            let db = Database::open(&path).unwrap();
            let id = db.insert(json!({"items": []})).unwrap();
            for i in 0..100 {
                db.array_push(&id, "items", json!({"v": i})).unwrap();
            }
            for _ in 0..50 {
                db.remove(&id, "items.0").unwrap();
            }
            for i in 0..50 {
                db.set(&id, &format!("items.{}.v", i), json!(i * 100)).unwrap();
            }
            db.flush().unwrap();
            db.compact().unwrap();

            let doc = db.get(&id).unwrap();
            assert_eq!(doc["items"].as_array().unwrap().len(), 50);
            assert_eq!(doc["items"][0]["v"], 0);
            assert_eq!(doc["items"][49]["v"], 4900);
            id
        };

        let db2 = Database::open(&path).unwrap();
        let doc = db2.get(&id).unwrap();
        assert_eq!(doc["items"].as_array().unwrap().len(), 50);
        assert_eq!(doc["items"][0]["v"], 0);
        assert_eq!(doc["items"][49]["v"], 4900);
    }

    #[test]
    fn stress_multiple_docs_concurrent_ops() {
        let (db, _dir) = test_db();
        let mut ids = Vec::new();
        for d in 0..10 {
            let id = db.insert(json!({
                "doc_id": d,
                "counter": 0,
                "tags": [],
                "meta": {"active": true}
            })).unwrap();
            ids.push(id);
        }

        for round in 0..50 {
            for (idx, id) in ids.iter().enumerate() {
                db.set(id, "counter", json!(round + 1)).unwrap();
                db.array_push(id, "tags", json!(format!("r{}_{}", round, idx))).unwrap();
                if round % 10 == 0 {
                    db.set(id, "meta.active", json!(round % 20 != 0)).unwrap();
                }
            }
        }

        for (idx, id) in ids.iter().enumerate() {
            let doc = db.get(id).unwrap();
            assert_eq!(doc["counter"], 50);
            assert_eq!(doc["tags"].as_array().unwrap().len(), 50);
            assert_eq!(doc["tags"][0], format!("r0_{}", idx));
            assert_eq!(doc["meta"]["active"], false);
        }
    }

    #[test]
    fn stress_remove_shifts_correctness() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"nums": [0, 1, 2, 3, 4, 5, 6, 7, 8, 9]})).unwrap();
        db.remove(&id, "nums.0").unwrap();
        assert_eq!(db.get(&id).unwrap()["nums"], json!([1, 2, 3, 4, 5, 6, 7, 8, 9]));
        db.remove(&id, "nums.0").unwrap();
        assert_eq!(db.get(&id).unwrap()["nums"], json!([2, 3, 4, 5, 6, 7, 8, 9]));
        db.remove(&id, "nums.7").unwrap();
        assert_eq!(db.get(&id).unwrap()["nums"], json!([2, 3, 4, 5, 6, 7, 8]));
        db.remove(&id, "nums.3").unwrap();
        assert_eq!(db.get(&id).unwrap()["nums"], json!([2, 3, 4, 6, 7, 8]));
    }

    #[test]
    fn stress_remove_all_array_elements() {
        let (db, _dir) = test_db();
        let id = db.insert(json!({"items": [1, 2, 3]})).unwrap();
        db.remove(&id, "items.0").unwrap();
        db.remove(&id, "items.0").unwrap();
        db.remove(&id, "items.0").unwrap();
        let doc = db.get(&id).unwrap();
        assert_eq!(doc["items"], json!([]));
    }
}
