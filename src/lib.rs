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
pub mod error;
pub mod id;
pub mod storage;

pub use bucket::{FileBucket, FileMeta, FileRef};
pub use error::{Error, Result};

use parking_lot::{Mutex, RwLock};
use serde_json::Value;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use id::{generate_unique, generate_unique_with_prefix};

// ─── Persistence Modes ──────────────────────────────────────────────

/// When to persist data to disk.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Persistence {
    /// Flush on explicit call or shutdown. Fastest, last flush only.
    Lazy,
    /// Flush every N seconds. Balanced.
    Scheduled(Duration),
    /// fsync after every write. Slowest, every write safe.
    Immediate,
}

impl Default for Persistence {
    fn default() -> Self {
        Persistence::Lazy
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

// ─── Index Types ────────────────────────────────────────────────────

/// Trait for secondary indexes.
trait Index: Send + Sync {
    fn insert(&mut self, value: &Value, id: &str);
    fn remove(&mut self, value: &Value, id: &str);
    fn get(&self, value: &Value) -> Vec<String>;
}

/// Hash index for O(1) equality lookups.
struct HashIndex {
    map: HashMap<String, HashSet<String>>,
}

impl HashIndex {
    fn new() -> Self {
        HashIndex {
            map: HashMap::new(),
        }
    }

    fn value_key(v: &Value) -> String {
        // Canonical string representation for HashMap key
        match v {
            Value::Null => "null".to_string(),
            Value::Bool(b) => b.to_string(),
            Value::Number(n) => n.to_string(),
            Value::String(s) => s.clone(),
            _ => v.to_string(), // arrays/objects use JSON string
        }
    }
}

impl Index for HashIndex {
    fn insert(&mut self, value: &Value, id: &str) {
        let key = Self::value_key(value);
        self.map.entry(key).or_default().insert(id.to_string());
    }

    fn remove(&mut self, value: &Value, id: &str) {
        let key = Self::value_key(value);
        if let Some(set) = self.map.get_mut(&key) {
            set.remove(id);
            if set.is_empty() {
                self.map.remove(&key);
            }
        }
    }

    fn get(&self, value: &Value) -> Vec<String> {
        let key = Self::value_key(value);
        self.map.get(&key).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }
}

/// BTree index for O(log n) lookups + range queries.
struct BTreeIndex {
    map: BTreeMap<String, HashSet<String>>,
}

impl BTreeIndex {
    fn new() -> Self {
        BTreeIndex {
            map: BTreeMap::new(),
        }
    }

    fn value_key(v: &Value) -> String {
        // Pad numbers for correct BTree ordering
        match v {
            Value::Number(n) => {
                if let Some(f) = n.as_f64() {
                    format!("{:020.10}", f)
                } else {
                    n.to_string()
                }
            }
            Value::String(s) => s.clone(),
            Value::Null => "\x00".to_string(),
            Value::Bool(b) => (if *b { "\x02" } else { "\x01" }).to_string(),
            _ => v.to_string(),
        }
    }
}

impl Index for BTreeIndex {
    fn insert(&mut self, value: &Value, id: &str) {
        let key = Self::value_key(value);
        self.map.entry(key).or_default().insert(id.to_string());
    }

    fn remove(&mut self, value: &Value, id: &str) {
        let key = Self::value_key(value);
        if let Some(set) = self.map.get_mut(&key) {
            set.remove(id);
            if set.is_empty() {
                self.map.remove(&key);
            }
        }
    }

    fn get(&self, value: &Value) -> Vec<String> {
        let key = Self::value_key(value);
        self.map.get(&key).map(|s| s.iter().cloned().collect()).unwrap_or_default()
    }
}

// ─── Query Evaluator ────────────────────────────────────────────────

/// Evaluate a JSON AST query against a document.
fn query_matches(doc: &Value, ast: &Value) -> bool {
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
fn field_get<'a>(doc: &'a Value, field: &str) -> Option<&'a Value> {
    let parts: Vec<&str> = field.split('.').collect();
    let mut current = doc;
    for part in parts {
        current = current.get(part)?;
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
                "$in" => operand
                    .as_array()
                    .map(|arr| arr.iter().any(|v| values_equal(field_val, v)))
                    .unwrap_or(false),
                "$nin" => operand
                    .as_array()
                    .map(|arr| !arr.iter().any(|v| values_equal(field_val, v)))
                    .unwrap_or(true),
                "$exists" => operand.as_bool().unwrap_or(true),
                _ => true, // Unknown operator = no filter
            })
        }
        // Implicit $eq: {"field": "value"}
        _ => values_equal(field_val, condition),
    }
}

/// Compare two JSON values for equality.
fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(an), Value::Number(bn)) => {
            // Compare as f64 for cross-type number equality
            an.as_f64() == bn.as_f64()
        }
        _ => a == b,
    }
}

/// Compare two JSON values for ordering.
fn value_cmp(a: &Value, b: &Value) -> std::cmp::Ordering {
    match (a, b) {
        (Value::Number(an), Value::Number(bn)) => {
            let af = an.as_f64().unwrap_or(0.0);
            let bf = bn.as_f64().unwrap_or(0.0);
            af.partial_cmp(&bf).unwrap_or(std::cmp::Ordering::Equal)
        }
        (Value::String(as_), Value::String(bs)) => as_.cmp(bs),
        (Value::Bool(ab), Value::Bool(bb)) => ab.cmp(bb),
        _ => std::cmp::Ordering::Equal,
    }
}

// ─── Database ───────────────────────────────────────────────────────

/// The main nDB database.
///
/// In-memory document store backed by JSON Lines persistence.
/// Single-writer, multi-reader concurrency model.
pub struct Database {
    /// Path to the JSONL data file.
    path: PathBuf,
    /// Base directory for the database (contains active/, _trash/, _files/).
    base_dir: PathBuf,
    /// In-memory document store: _id → document.
    docs: RwLock<HashMap<String, Value>>,
    /// Set of deleted document IDs (tombstones).
    deleted: RwLock<HashSet<String>>,
    /// Secondary indexes (opt-in).
    indexes: RwLock<HashMap<String, Box<dyn Index>>>,
    /// Single-writer mutex.
    writer: Mutex<()>,
    /// Persistence mode.
    persistence: Persistence,
    /// Trash mode.
    trash_mode: TrashMode,
    /// Append-only file handle (held open for writes).
    file_handle: Mutex<Option<fs::File>>,
}

impl Database {
    /// Open or create a database at the given path.
    ///
    /// If the file exists, loads all documents into memory.
    /// If not, creates a new file with _meta header.
    pub fn open(path: impl AsRef<Path>) -> Result<Self> {
        let path = path.as_ref().to_path_buf();
        let base_dir = path.parent().unwrap_or(Path::new(".")).to_path_buf();

        // Ensure file exists
        if !path.exists() {
            storage::init_file(&path)?;
        }

        // Load all documents from file
        let raw_docs = storage::read_all(&path)?;

        // Build in-memory state: last write wins
        let mut docs: HashMap<String, Value> = HashMap::new();
        let mut deleted = HashSet::new();

        for doc in raw_docs {
            if let Some(id) = doc.get("_id").and_then(|v| v.as_str()) {
                if doc.get("_deleted").is_some() {
                    // Tombstone entry
                    deleted.insert(id.to_string());
                    docs.remove(id);
                } else if let Some("array_push") = doc.get("_op").and_then(|v| v.as_str()) {
                    // Array push patch
                    if let Some(field) = doc.get("field").and_then(|v| v.as_str()) {
                        if let Some(value) = doc.get("value") {
                            if let Some(existing) = docs.get_mut(id) {
                                if let Some(obj) = existing.as_object_mut() {
                                    if let Some(arr) = obj.get_mut(field).and_then(|v| v.as_array_mut()) {
                                        arr.push(value.clone());
                                    } else {
                                        obj.insert(field.to_string(), serde_json::json!([value.clone()]));
                                    }
                                }
                            }
                        }
                    }
                } else {
                    deleted.remove(id);
                    docs.insert(id.to_string(), doc);
                }
            }
        }

        Ok(Database {
            path,
            base_dir,
            docs: RwLock::new(docs),
            deleted: RwLock::new(deleted),
            indexes: RwLock::new(HashMap::new()),
            writer: Mutex::new(()),
            persistence: Persistence::Lazy,
            trash_mode: TrashMode::Manual,
            file_handle: Mutex::new(None),
        })
    }

    /// Open a purely in-memory database (no disk file).
    pub fn open_in_memory() -> Result<Self> {
        Ok(Database {
            path: PathBuf::new(),
            base_dir: PathBuf::new(),
            docs: RwLock::new(HashMap::new()),
            deleted: RwLock::new(HashSet::new()),
            indexes: RwLock::new(HashMap::new()),
            writer: Mutex::new(()),
            persistence: Persistence::Lazy,
            trash_mode: TrashMode::Manual,
            file_handle: Mutex::new(None),
        })
    }

    /// Set persistence mode. Returns self for chaining.
    pub fn with_persistence(mut self, mode: Persistence) -> Self {
        self.persistence = mode;
        self
    }

    /// Set trash mode. Returns self for chaining.
    pub fn with_trash_mode(mut self, mode: TrashMode) -> Self {
        self.trash_mode = mode;
        self
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

    // ─── Layer 1: Core Operations ──────────────────────────────────

    /// Insert a document. Generates a NanoID `_id` and returns it.
    /// O(1) operation: HashMap insert + file append.
    pub fn insert(&self, mut doc: Value) -> Result<String> {
        let _guard = self.writer.lock();

        let docs_reader = self.docs.read();
        let existing: HashSet<String> = docs_reader.keys().cloned().collect();
        drop(docs_reader);

        let id = generate_unique(&existing);
        doc.as_object_mut()
            .unwrap()
            .insert("_id".to_string(), Value::String(id.clone()));

        // Append to file
        if !self.is_in_memory() {
            let line = serde_json::to_string(&doc)?;
            let mut handle = self.get_file_handle()?;
            if let Some(ref mut file) = *handle {
                match self.persistence {
                    Persistence::Immediate => {
                        storage::append_line_sync(file, &self.path, &line)?;
                    }
                    _ => {
                        storage::append_line(file, &self.path, &line)?;
                    }
                }
            }
        }

        // Update indexes
        let mut indexes = self.indexes.write();
        for (field, index) in indexes.iter_mut() {
            if let Some(val) = doc.get(field) {
                index.insert(val, &id);
            }
        }
        drop(indexes);

        // Update in-memory store
        let mut docs = self.docs.write();
        self.deleted.write().remove(&id);
        docs.insert(id.clone(), doc);

        Ok(id)
    }

    /// Insert a document with a prefixed ID.
    pub fn insert_with_prefix(&self, prefix: &str, mut doc: Value) -> Result<String> {
        let _guard = self.writer.lock();

        let docs_reader = self.docs.read();
        let existing: HashSet<String> = docs_reader.keys().cloned().collect();
        drop(docs_reader);

        let id = generate_unique_with_prefix(prefix, &existing);
        doc.as_object_mut()
            .unwrap()
            .insert("_id".to_string(), Value::String(id.clone()));

        if !self.is_in_memory() {
            let line = serde_json::to_string(&doc)?;
            let mut handle = self.get_file_handle()?;
            if let Some(ref mut file) = *handle {
                match self.persistence {
                    Persistence::Immediate => {
                        storage::append_line_sync(file, &self.path, &line)?;
                    }
                    _ => {
                        storage::append_line(file, &self.path, &line)?;
                    }
                }
            }
        }

        let mut indexes = self.indexes.write();
        for (field, index) in indexes.iter_mut() {
            if let Some(val) = doc.get(field) {
                index.insert(val, &id);
            }
        }
        drop(indexes);

        let mut docs = self.docs.write();
        self.deleted.write().remove(&id);
        docs.insert(id.clone(), doc);

        Ok(id)
    }

    /// Get a document by ID. O(1) HashMap lookup.
    pub fn get(&self, id: &str) -> Result<Value> {
        let docs = self.docs.read();
        docs.get(id)
            .cloned()
            .ok_or_else(|| Error::not_found(id))
    }

    /// Update a document. Appends new version to file, old version superseded.
    /// O(1) operation.
    pub fn update(&self, id: &str, mut new_doc: Value) -> Result<()> {
        let _guard = self.writer.lock();

        {
            let docs = self.docs.read();
            if !docs.contains_key(id) {
                return Err(Error::not_found(id));
            }
        }

        // Set _id on new doc
        new_doc
            .as_object_mut()
            .unwrap()
            .insert("_id".to_string(), Value::String(id.to_string()));

        // Remove old values from indexes, add new
        let mut indexes = self.indexes.write();
        {
            let docs = self.docs.read();
            if let Some(old_doc) = docs.get(id) {
                for (field, index) in indexes.iter_mut() {
                    if let Some(old_val) = old_doc.get(field) {
                        index.remove(old_val, id);
                    }
                }
            }
        }
        for (field, index) in indexes.iter_mut() {
            if let Some(val) = new_doc.get(field) {
                index.insert(val, id);
            }
        }
        drop(indexes);

        // Append to file
        if !self.is_in_memory() {
            let line = serde_json::to_string(&new_doc)?;
            let mut handle = self.get_file_handle()?;
            if let Some(ref mut file) = *handle {
                match self.persistence {
                    Persistence::Immediate => {
                        storage::append_line_sync(file, &self.path, &line)?;
                    }
                    _ => {
                        storage::append_line(file, &self.path, &line)?;
                    }
                }
            }
        }

        // Update in-memory store
        let mut docs = self.docs.write();
        docs.insert(id.to_string(), new_doc);

        Ok(())
    }

    /// Append an element to an array field. O(1) file write.
    pub fn array_push(&self, id: &str, field: &str, value: Value) -> Result<()> {
        let _guard = self.writer.lock();

        {
            let mut docs = self.docs.write();
            if let Some(doc) = docs.get_mut(id) {
                if let Some(obj) = doc.as_object_mut() {
                    if let Some(arr) = obj.get_mut(field).and_then(|v| v.as_array_mut()) {
                        arr.push(value.clone());
                    } else {
                        obj.insert(field.to_string(), serde_json::json!([value.clone()]));
                    }
                }
            } else {
                return Err(Error::not_found(id));
            }
        }

        // Write patch to file
        if !self.is_in_memory() {
            let patch = serde_json::json!({
                "_id": id,
                "_op": "array_push",
                "field": field,
                "value": value
            });
            let line = serde_json::to_string(&patch)?;
            let mut handle = self.get_file_handle()?;
            if let Some(ref mut file) = *handle {
                match self.persistence {
                    Persistence::Immediate => {
                        storage::append_line_sync(file, &self.path, &line)?;
                    }
                    _ => {
                        storage::append_line(file, &self.path, &line)?;
                    }
                }
            }
        }

        Ok(())
    }

    /// Delete a document (soft delete / tombstone). O(1).
    pub fn delete(&self, id: &str) -> Result<()> {
        let _guard = self.writer.lock();

        {
            let docs = self.docs.read();
            if !docs.contains_key(id) {
                return Err(Error::not_found(id));
            }
        }

        // Remove from indexes
        let mut indexes = self.indexes.write();
        {
            let docs = self.docs.read();
            if let Some(doc) = docs.get(id) {
                for (field, index) in indexes.iter_mut() {
                    if let Some(val) = doc.get(field) {
                        index.remove(val, id);
                    }
                }
            }
        }
        drop(indexes);

        // Write tombstone to file
        if !self.is_in_memory() {
            let now = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs();
            let tombstone = serde_json::json!({
                "_id": id,
                "_deleted": now
            });
            let line = serde_json::to_string(&tombstone)?;
            let mut handle = self.get_file_handle()?;
            if let Some(ref mut file) = *handle {
                match self.persistence {
                    Persistence::Immediate => {
                        storage::append_line_sync(file, &self.path, &line)?;
                    }
                    _ => {
                        storage::append_line(file, &self.path, &line)?;
                    }
                }
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
    /// Returns a Vec of cloned Values for thread safety.
    pub fn iter(&self) -> Vec<Value> {
        let docs = self.docs.read();
        docs.values().cloned().collect()
    }

    /// Number of active (non-deleted) documents.
    pub fn len(&self) -> usize {
        self.docs.read().len()
    }

    /// Check if database is empty.
    pub fn is_empty(&self) -> bool {
        self.docs.read().is_empty()
    }

    /// Check if a document exists.
    pub fn contains(&self, id: &str) -> bool {
        self.docs.read().contains_key(id)
    }

    // ─── Layer 2: Single Field Queries ─────────────────────────────

    /// Find all documents where `field` equals `value`.
    /// Uses index if available, otherwise linear scan.
    pub fn find(&self, field: &str, value: &Value) -> Vec<Value> {
        // Check for index
        {
            let indexes = self.indexes.read();
            if let Some(index) = indexes.get(field) {
                let ids = index.get(value);
                let docs = self.docs.read();
                return ids
                    .iter()
                    .filter_map(|id| docs.get(id).cloned())
                    .collect();
            }
        }

        // Linear scan
        let docs = self.docs.read();
        docs.values()
            .filter(|doc| {
                doc.get(field)
                    .map(|v| values_equal(v, value))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// Find documents where field matches a predicate closure.
    pub fn find_where<F>(&self, field: &str, predicate: F) -> Vec<Value>
    where
        F: Fn(&Value) -> bool,
    {
        let docs = self.docs.read();
        docs.values()
            .filter(|doc| {
                doc.get(field)
                    .map(|v| predicate(v))
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    /// Find documents with field value in a range (requires BTree index).
    /// Falls back to linear scan if no BTree index exists.
    pub fn find_range(&self, field: &str, min: &Value, max: &Value) -> Vec<Value> {
        let docs = self.docs.read();
        docs.values()
            .filter(|doc| {
                doc.get(field)
                    .map(|v| {
                        value_cmp(v, min) != std::cmp::Ordering::Less
                            && value_cmp(v, max) != std::cmp::Ordering::Greater
                    })
                    .unwrap_or(false)
            })
            .cloned()
            .collect()
    }

    // ─── Layer 3: JSON AST Queries ─────────────────────────────────

    /// Execute a JSON AST query. Returns all matching documents.
    pub fn query(&self, ast: Value) -> Vec<Value> {
        let docs = self.docs.read();
        docs.values()
            .filter(|doc| query_matches(doc, &ast))
            .cloned()
            .collect()
    }

    /// Execute a JSON AST query with options (limit, sort, offset).
    pub fn query_with(&self, ast: Value, opts: QueryOptions) -> Vec<Value> {
        let mut results = self.query(ast);

        // Sort
        if let Some((ref field, dir)) = opts.sort_by {
            results.sort_by(|a, b| {
                let av = a.get(field).unwrap_or(&Value::Null);
                let bv = b.get(field).unwrap_or(&Value::Null);
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

        results
    }

    // ─── Index Management ──────────────────────────────────────────

    /// Create a hash index on a field. Scans all documents once.
    pub fn create_index(&self, field: &str) -> Result<()> {
        let _guard = self.writer.lock();

        let mut index = HashIndex::new();
        let docs = self.docs.read();
        for (id, doc) in docs.iter() {
            if let Some(val) = doc.get(field) {
                index.insert(val, id);
            }
        }

        self.indexes
            .write()
            .insert(field.to_string(), Box::new(index));
        Ok(())
    }

    /// Create a BTree index on a field (for range queries).
    pub fn create_btree_index(&self, field: &str) -> Result<()> {
        let _guard = self.writer.lock();

        let mut index = BTreeIndex::new();
        let docs = self.docs.read();
        for (id, doc) in docs.iter() {
            if let Some(val) = doc.get(field) {
                index.insert(val, id);
            }
        }

        self.indexes
            .write()
            .insert(field.to_string(), Box::new(index));
        Ok(())
    }

    /// Drop an index, freeing memory.
    pub fn drop_index(&self, field: &str) -> Result<()> {
        let mut indexes = self.indexes.write();
        indexes
            .remove(field)
            .ok_or_else(|| Error::index_error(field, "index not found"))?;
        Ok(())
    }

    /// Check if an index exists for a field.
    pub fn has_index(&self, field: &str) -> bool {
        self.indexes.read().contains_key(field)
    }

    // ─── Compaction & Trash ────────────────────────────────────────

    /// Compact the database: rewrite active docs, archive deleted to trash.
    pub fn compact(&self) -> Result<()> {
        let _guard = self.writer.lock();

        if self.is_in_memory() {
            return Ok(());
        }

        // Close file handle before rewrite
        {
            let mut handle = self.file_handle.lock();
            *handle = None;
        }

        let docs = self.docs.read();
        let active: Vec<&Value> = docs.values().collect();

        // Collect deleted docs from the file (re-read to find tombstones)
        let raw_docs = storage::read_all(&self.path)?;
        let mut trash_docs: Vec<&Value> = Vec::new();
        for doc in &raw_docs {
            if doc.get("_deleted").is_some() {
                trash_docs.push(doc);
            }
        }

        // Archive deleted to trash
        if !trash_docs.is_empty() {
            let trash_dir = self.base_dir.join("_trash").join("docs");
            let collection_name = self
                .path
                .file_stem()
                .unwrap_or_default()
                .to_string_lossy()
                .to_string();
            storage::append_trash(&trash_dir, &collection_name, &trash_docs)?;
        }

        // Rewrite active docs
        storage::rewrite_atomic(&self.path, &active)?;

        Ok(())
    }

    /// Export a pristine snapshot of the database to a target directory.
    /// Functions similarly to compaction, but writes exactly the active 
    /// docs, metadata, and files to a completely new folder without trash.
    pub fn export_snapshot<P: AsRef<Path>>(&self, target_dir: P) -> Result<()> {
        let _guard = self.writer.lock();
        let target = target_dir.as_ref();
        
        let docs = self.docs.read();
        let active: Vec<&Value> = docs.values().collect();
        
        // 1. Write the clean JSON line docs
        let target_db = target.join(self.path.file_name().unwrap());
        storage::rewrite_atomic(&target_db, &active)?;
        
        // 2. Copy the meta.json across
        let meta_src = self.base_dir.join("meta.json");
        let meta_dst = target.join("meta.json");
        if meta_src.exists() {
            fs::copy(&meta_src, &meta_dst).map_err(Error::io_err(&meta_dst, "copy meta.json for snapshot"))?;
        }
        
        // 3. Create the clean buckets folder & stream the active files
        let buckets_src = self.base_dir.join("buckets");
        let buckets_dst = target.join("buckets");
        if buckets_src.exists() {
            // Find only buckets natively active in this DB
            for entry in fs::read_dir(&buckets_src).map_err(Error::io_err(&buckets_src, "read buckets dir"))? {
                let entry = entry.map_err(Error::io_err(&buckets_src, "read bucket entry"))?;
                if entry.file_type().map_or(false, |t| t.is_dir()) {
                    let bucket_name = entry.file_name();
                    let dst_bucket = buckets_dst.join(&bucket_name);
                    fs::create_dir_all(&dst_bucket).map_err(Error::io_err(&dst_bucket, "create snapshot bucket"))?;
                    
                    // Copy non-trash binaries
                    for file_entry in fs::read_dir(entry.path()).map_err(Error::io_err(&entry.path(), "read bucket files"))? {
                        let f = file_entry.map_err(Error::io_err(&entry.path(), "read active binary"))?;
                        let file_name = f.file_name();
                        if file_name != "_trash" && f.file_type().map_or(false, |t| t.is_file()) {
                            fs::copy(f.path(), dst_bucket.join(&file_name)).map_err(Error::io_err(f.path(), "copy file to snapshot"))?;
                        }
                    }
                }
            }
        }
        
        Ok(())
    }

    /// Get the trash directory path.
    pub fn trash_dir(&self) -> PathBuf {
        self.base_dir.join("_trash").join("docs")
    }

    /// List deleted document IDs.
    pub fn deleted_ids(&self) -> Vec<String> {
        self.deleted.read().iter().cloned().collect()
    }

    /// Get all active document IDs.
    pub fn get_all_ids(&self) -> Vec<String> {
        self.docs.read().keys().cloned().collect()
    }

    /// Restore a deleted document from trash by ID.
    /// Re-reads the file to find the last non-deleted version.
    pub fn restore(&self, id: &str) -> Result<()> {
        let _guard = self.writer.lock();

        if self.is_in_memory() {
            return Err(Error::invalid_arg("cannot restore in in-memory database"));
        }

        // Read file to find the document before tombstone
        let raw_docs = storage::read_all(&self.path)?;
        let mut last_version: Option<Value> = None;
        for doc in &raw_docs {
            if doc.get("_id").and_then(|v| v.as_str()) == Some(id) {
                if doc.get("_deleted").is_some() {
                    // Tombstone — stop looking
                    break;
                }
                last_version = Some(doc.clone());
            }
        }

        let doc = last_version.ok_or_else(|| Error::not_found(id))?;

        // Append restored doc to file
        let line = serde_json::to_string(&doc)?;
        let mut handle = self.get_file_handle()?;
        if let Some(ref mut file) = *handle {
            match self.persistence {
                Persistence::Immediate => {
                    storage::append_line_sync(file, &self.path, &line)?;
                }
                _ => {
                    storage::append_line(file, &self.path, &line)?;
                }
            }
        }

        // Update in-memory state
        self.deleted.write().remove(id);
        self.docs.write().insert(id.to_string(), doc);

        Ok(())
    }

    // ─── Persistence ───────────────────────────────────────────────

    /// Explicitly flush pending writes to disk.
    pub fn flush(&self) -> Result<()> {
        if self.is_in_memory() {
            return Ok(());
        }

        let mut handle = self.file_handle.lock();
        if let Some(ref mut file) = *handle {
            file.flush()
                .map_err(Error::io_err(&self.path, "flush"))?;
            file.sync_all()
                .map_err(Error::io_err(&self.path, "fsync"))?;
        }

        Ok(())
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
}

impl Error {
    fn index_error(field: impl Into<String>, reason: impl Into<String>) -> Self {
        Error::IndexError {
            field: field.into(),
            reason: reason.into(),
        }
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

        let results = db.query(json!({"status": {"$eq": "active"}}));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["name"], "a");
    }

    #[test]
    fn query_implicit_eq() {
        let (db, _dir) = test_db();
        db.insert(json!({"color": "red"})).unwrap();
        db.insert(json!({"color": "blue"})).unwrap();

        let results = db.query(json!({"color": "red"}));
        assert_eq!(results.len(), 1);
    }

    #[test]
    fn query_comparison_ops() {
        let (db, _dir) = test_db();
        db.insert(json!({"score": 10})).unwrap();
        db.insert(json!({"score": 50})).unwrap();
        db.insert(json!({"score": 100})).unwrap();

        let gt = db.query(json!({"score": {"$gt": 40}}));
        assert_eq!(gt.len(), 2);

        let lt = db.query(json!({"score": {"$lt": 60}}));
        assert_eq!(lt.len(), 2);

        let gte = db.query(json!({"score": {"$gte": 50}}));
        assert_eq!(gte.len(), 2);

        let lte = db.query(json!({"score": {"$lte": 50}}));
        assert_eq!(lte.len(), 2);
    }

    #[test]
    fn query_ne() {
        let (db, _dir) = test_db();
        db.insert(json!({"x": 1})).unwrap();
        db.insert(json!({"x": 2})).unwrap();

        let results = db.query(json!({"x": {"$ne": 1}}));
        assert_eq!(results.len(), 1);
        assert_eq!(results[0]["x"], 2);
    }

    #[test]
    fn query_in() {
        let (db, _dir) = test_db();
        db.insert(json!({"status": "active"})).unwrap();
        db.insert(json!({"status": "pending"})).unwrap();
        db.insert(json!({"status": "deleted"})).unwrap();

        let results = db.query(json!({"status": {"$in": ["active", "pending"]}}));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_nin() {
        let (db, _dir) = test_db();
        db.insert(json!({"status": "active"})).unwrap();
        db.insert(json!({"status": "pending"})).unwrap();
        db.insert(json!({"status": "deleted"})).unwrap();

        let results = db.query(json!({"status": {"$nin": ["deleted"]}}));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_exists() {
        let (db, _dir) = test_db();
        db.insert(json!({"name": "a", "avatar": "yes"})).unwrap();
        db.insert(json!({"name": "b"})).unwrap();

        let exists = db.query(json!({"avatar": {"$exists": true}}));
        assert_eq!(exists.len(), 1);

        let not_exists = db.query(json!({"avatar": {"$exists": false}}));
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
        }));
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
        }));
        assert_eq!(results.len(), 2);
    }

    #[test]
    fn query_not_combinator() {
        let (db, _dir) = test_db();
        db.insert(json!({"status": "active"})).unwrap();
        db.insert(json!({"status": "deleted"})).unwrap();

        let results = db.query(json!({
            "$not": {"status": {"$eq": "deleted"}}
        }));
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
        );
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
        assert_eq!(db.query(json!({"x": 1})).len(), 0);
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
}
