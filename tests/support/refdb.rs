//! Sequential reference model of the nDB document semantics.
//!
//! `RefDb` is deliberately the dumbest thing that can be correct: one
//! `BTreeMap` of owned `serde_json::Value`s, one append-only event vector, and
//! linear scans. No indexes, no sharing, no async, no group commit, no disk.
//! Everything the engine promises about *observable state* is decided here by
//! the simplest possible code, so any disagreement with `Database` is either an
//! engine bug or a contract ambiguity — never a bug in the model's cleverness.
//!
//! Semantics are matched against the **RAM-first engine**
//! (`Database::open_ram`), which is the only binary backend nDB ships. Where
//! v1 and the RAM-first engine differ, the RAM-first behaviour wins.
//!
//! Ordering model: an unsorted scan is id-ascending, and a sorted query breaks
//! ties by that scan position. `RefDb` therefore scans id-ascending and breaks
//! sort ties by id.

use ndb::{ChangeBatch, ChangeEvent, ChangeKind, QueryOptions, Snapshot, SortDir, SourceLineage};
use serde_json::{Map, Value};
use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

/// The error classes the reference model distinguishes. One variant per
/// observable behaviour a caller can react to — never a message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RefError {
    NotFound,
    InvalidArgument,
    Conflict {
        current_seq: u64,
    },
    CursorExpired {
        requested_seq: u64,
        min_available_seq: u64,
        last_seq: u64,
    },
    CursorAhead {
        requested_seq: u64,
        last_seq: u64,
    },
}

impl RefError {
    /// Map a production error onto the same class, so a test compares
    /// behaviour instead of prose.
    pub fn from_ndb(error: &ndb::Error) -> RefError {
        match error {
            ndb::Error::NotFound { .. } => RefError::NotFound,
            ndb::Error::InvalidArgument { .. } => RefError::InvalidArgument,
            ndb::Error::Conflict { current_seq, .. } => RefError::Conflict {
                current_seq: *current_seq,
            },
            ndb::Error::CursorExpired {
                requested_seq,
                min_available_seq,
                last_seq,
            } => RefError::CursorExpired {
                requested_seq: *requested_seq,
                min_available_seq: *min_available_seq,
                last_seq: *last_seq,
            },
            ndb::Error::CursorAhead {
                requested_seq,
                last_seq,
            } => RefError::CursorAhead {
                requested_seq: *requested_seq,
                last_seq: *last_seq,
            },
            other => panic!("reference model has no class for production error: {other}"),
        }
    }
}

pub type RefResult<T> = Result<T, RefError>;

const KNOWN_OPERATORS: [&str; 9] = [
    "$eq", "$ne", "$gt", "$gte", "$lt", "$lte", "$in", "$nin", "$exists",
];

// ─── Total order over JSON values (mirrors OrderedValue) ────────────────────

#[derive(Debug, Clone, Copy)]
struct OrdF64(f64);

impl PartialEq for OrdF64 {
    fn eq(&self, other: &Self) -> bool {
        self.0.total_cmp(&other.0) == Ordering::Equal
    }
}
impl Eq for OrdF64 {}
impl PartialOrd for OrdF64 {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}
impl Ord for OrdF64 {
    fn cmp(&self, other: &Self) -> Ordering {
        self.0.total_cmp(&other.0)
    }
}

/// Null < Bool < Number < String < array/object, numbers by `total_cmp`,
/// arrays/objects by their canonical JSON text.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
enum OrdValue {
    Null,
    Bool(bool),
    Number(OrdF64),
    Str(String),
    Composite(String),
}

fn ordered(value: &Value) -> OrdValue {
    match value {
        Value::Null => OrdValue::Null,
        Value::Bool(flag) => OrdValue::Bool(*flag),
        Value::Number(number) => OrdValue::Number(OrdF64(
            number
                .as_f64()
                .expect("serde_json number is not representable as f64"),
        )),
        Value::String(text) => OrdValue::Str(text.clone()),
        other => OrdValue::Composite(other.to_string()),
    }
}

fn value_cmp(a: &Value, b: &Value) -> Ordering {
    ordered(a).cmp(&ordered(b))
}

fn values_equal(a: &Value, b: &Value) -> bool {
    match (a, b) {
        (Value::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        _ => a == b,
    }
}

// ─── Field access and query evaluation ─────────────────────────────────────

fn field_get<'a>(doc: &'a Value, field: &str) -> Option<&'a Value> {
    let mut current = doc;
    for part in field.split('.') {
        current = match current {
            Value::Array(items) => items.get(part.parse::<usize>().ok()?)?,
            other => other.get(part)?,
        };
    }
    Some(current)
}

fn evaluate_condition(field_value: &Value, condition: &Value) -> bool {
    match condition {
        Value::Object(ops) => ops.iter().all(|(op, operand)| match op.as_str() {
            "$eq" => values_equal(field_value, operand),
            "$ne" => !values_equal(field_value, operand),
            "$gt" => value_cmp(field_value, operand) == Ordering::Greater,
            "$gte" => value_cmp(field_value, operand) != Ordering::Less,
            "$lt" => value_cmp(field_value, operand) == Ordering::Less,
            "$lte" => value_cmp(field_value, operand) != Ordering::Greater,
            "$in" => operand
                .as_array()
                .map(|wanted| match field_value {
                    Value::Array(items) => items
                        .iter()
                        .any(|element| wanted.iter().any(|v| values_equal(element, v))),
                    scalar => wanted.iter().any(|v| values_equal(scalar, v)),
                })
                .unwrap_or(false),
            "$nin" => operand
                .as_array()
                .map(|wanted| {
                    let hit = match field_value {
                        Value::Array(items) => items
                            .iter()
                            .any(|element| wanted.iter().any(|v| values_equal(element, v))),
                        scalar => wanted.iter().any(|v| values_equal(scalar, v)),
                    };
                    !hit
                })
                .unwrap_or(true),
            "$exists" => operand.as_bool().unwrap_or(true),
            _ => true,
        }),
        implicit => values_equal(field_value, implicit),
    }
}

fn query_matches(doc: &Value, ast: &Value) -> bool {
    match ast {
        Value::Object(map) => {
            if let Some(expr) = map.get("$and") {
                return expr
                    .as_array()
                    .map(|conditions| conditions.iter().all(|c| query_matches(doc, c)))
                    .unwrap_or(false);
            }
            if let Some(expr) = map.get("$or") {
                return expr
                    .as_array()
                    .map(|conditions| conditions.iter().any(|c| query_matches(doc, c)))
                    .unwrap_or(false);
            }
            if let Some(expr) = map.get("$not") {
                return !query_matches(doc, expr);
            }
            map.iter().all(|(field, condition)| {
                match field_get(doc, field) {
                    None => match condition {
                        Value::Object(ops) => match ops.get("$exists") {
                            Some(exists) => !exists.as_bool().unwrap_or(true),
                            None => false,
                        },
                        _ => false,
                    },
                    Some(value) => evaluate_condition(value, condition),
                }
            })
        }
        Value::Array(conditions) => conditions.iter().all(|c| query_matches(doc, c)),
        _ => false,
    }
}

fn validate_ast(ast: &Value) -> RefResult<()> {
    match ast {
        Value::Object(map) => {
            for (key, value) in map {
                match key.as_str() {
                    "$and" | "$or" => {
                        let Some(conditions) = value.as_array() else {
                            return Err(RefError::InvalidArgument);
                        };
                        if conditions.is_empty() {
                            return Err(RefError::InvalidArgument);
                        }
                        for condition in conditions {
                            validate_ast(condition)?;
                        }
                    }
                    "$not" => validate_ast(value)?,
                    operator if operator.starts_with('$') => return Err(RefError::InvalidArgument),
                    _ => {
                        if let Value::Object(ops) = value {
                            for operator in ops.keys() {
                                if !KNOWN_OPERATORS.contains(&operator.as_str()) {
                                    return Err(RefError::InvalidArgument);
                                }
                            }
                        }
                    }
                }
            }
            Ok(())
        }
        Value::Array(conditions) => {
            for condition in conditions {
                validate_ast(condition)?;
            }
            Ok(())
        }
        _ => Err(RefError::InvalidArgument),
    }
}

// ─── Path mutations (mirrors walk_and_set / push / remove) ─────────────────

fn apply_path_set(doc: &mut Value, path: &str, value: Value) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    walk_set(doc, &segments, 0, value)
}

fn walk_set(current: &mut Value, segments: &[&str], depth: usize, value: Value) -> bool {
    if depth >= segments.len() {
        return false;
    }
    let key = segments[depth];
    let last = depth + 1 == segments.len();
    if let Ok(index) = key.parse::<usize>() {
        if let Some(items) = current.as_array_mut() {
            if index < items.len() {
                if last {
                    items[index] = value;
                    return true;
                }
                return walk_set(&mut items[index], segments, depth + 1, value);
            }
        }
    } else if let Some(object) = current.as_object_mut() {
        if last {
            // Re-assigning the same value is an applied write, not a no-op.
            object.insert(key.to_string(), value);
            return true;
        }
        if object.contains_key(key) {
            return walk_set(object.get_mut(key).unwrap(), segments, depth + 1, value);
        }
    }
    false
}

fn apply_path_remove(doc: &mut Value, path: &str) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    walk_remove(doc, &segments, 0)
}

fn walk_remove(current: &mut Value, segments: &[&str], depth: usize) -> bool {
    if depth >= segments.len() {
        return false;
    }
    let key = segments[depth];
    let last = depth + 1 == segments.len();
    if let Ok(index) = key.parse::<usize>() {
        if let Some(items) = current.as_array_mut() {
            if index < items.len() {
                if last {
                    items.remove(index);
                    return true;
                }
                return walk_remove(&mut items[index], segments, depth + 1);
            }
        }
    } else if let Some(object) = current.as_object_mut() {
        if last {
            return object.remove(key).is_some();
        }
        if object.contains_key(key) {
            return walk_remove(object.get_mut(key).unwrap(), segments, depth + 1);
        }
    }
    false
}

fn apply_path_push(doc: &mut Value, path: &str, value: Value) -> bool {
    let segments: Vec<&str> = path.split('.').collect();
    walk_push(doc, &segments, 0, value)
}

fn walk_push(current: &mut Value, segments: &[&str], depth: usize, value: Value) -> bool {
    if depth >= segments.len() {
        return false;
    }
    let key = segments[depth];
    let last = depth + 1 == segments.len();
    if let Ok(index) = key.parse::<usize>() {
        if let Some(items) = current.as_array_mut() {
            if index < items.len() {
                if last {
                    return match items[index].as_array_mut() {
                        Some(inner) => {
                            inner.push(value);
                            true
                        }
                        None => false,
                    };
                }
                return walk_push(&mut items[index], segments, depth + 1, value);
            }
        }
        return false;
    }
    if let Some(object) = current.as_object_mut() {
        if last {
            return match object.get_mut(key) {
                Some(slot) => match slot.as_array_mut() {
                    Some(inner) => {
                        inner.push(value);
                        true
                    }
                    None => false,
                },
                None => {
                    object.insert(key.to_string(), Value::Array(vec![value]));
                    true
                }
            };
        }
        if object.contains_key(key) {
            return walk_push(object.get_mut(key).unwrap(), segments, depth + 1, value);
        }
    }
    false
}

// ─── The model ────────────────────────────────────────────────────────────

/// One mutation to replay. Mirrors `ndb::PatchOp` without depending on the
/// engine's own type (an oracle that shares types can share bugs).
#[derive(Debug, Clone)]
pub enum RefOp {
    Set { path: String, value: Value },
    Remove { path: String },
    ArrayPush { path: String, value: Value },
}

pub struct RefDb {
    docs: BTreeMap<String, Value>,
    doc_seqs: BTreeMap<String, u64>,
    deleted: BTreeSet<String>,
    /// Full documents as they were at each delete, oldest first.
    trash: Vec<Value>,
    events: Vec<ChangeEvent>,
    last_seq: u64,
    min_available_seq: u64,
    next_generated_id: u64,
}

impl Default for RefDb {
    fn default() -> Self {
        Self::new()
    }
}

impl RefDb {
    pub fn new() -> Self {
        RefDb {
            docs: BTreeMap::new(),
            doc_seqs: BTreeMap::new(),
            deleted: BTreeSet::new(),
            trash: Vec::new(),
            events: Vec::new(),
            last_seq: 0,
            min_available_seq: 1,
            next_generated_id: 1,
        }
    }

    // ── mutations ───────────────────────────────────────────────────────

    /// Standalone insert: generates `ref-N` ids. Differential tests use
    /// [`RefDb::insert_with_id`] so both sides store the same id.
    pub fn insert(&mut self, doc: Value) -> RefResult<String> {
        let id = format!("ref-{}", self.next_generated_id);
        self.next_generated_id += 1;
        self.insert_with_id(&id, doc)?;
        Ok(id)
    }

    /// Insert with the id the engine assigned. A duplicate id is a harness
    /// bug, and it fails loud rather than silently overwriting.
    pub fn insert_with_id(&mut self, id: &str, doc: Value) -> RefResult<()> {
        if self.docs.contains_key(id) || self.deleted.contains(id) {
            panic!("reference model: id {id} already exists in this database");
        }
        let mut object = match doc {
            Value::Object(object) => object,
            _ => return Err(RefError::InvalidArgument),
        };
        object.insert("_id".to_string(), Value::String(id.to_string()));
        object.remove("_seq");
        self.commit_upsert(id, Value::Object(object));
        Ok(())
    }

    pub fn update(&mut self, id: &str, doc: Value) -> RefResult<()> {
        self.update_if(id, doc, None)
    }

    pub fn update_if(&mut self, id: &str, doc: Value, if_seq: Option<u64>) -> RefResult<()> {
        if !self.docs.contains_key(id) {
            return Err(RefError::NotFound);
        }
        if let Some(expected) = if_seq {
            let current = self.doc_seq(id).unwrap_or(0);
            if expected != current {
                return Err(RefError::Conflict {
                    current_seq: current,
                });
            }
        }
        let mut object = match doc {
            Value::Object(object) => object,
            _ => return Err(RefError::InvalidArgument),
        };
        object.insert("_id".to_string(), Value::String(id.to_string()));
        object.remove("_seq");
        self.commit_upsert(id, Value::Object(object));
        Ok(())
    }

    pub fn set(&mut self, id: &str, path: &str, value: Value) -> RefResult<bool> {
        let mut staged = self.live(id)?.clone();
        if !apply_path_set(&mut staged, path, value) {
            return Ok(false);
        }
        self.commit_upsert(id, staged);
        Ok(true)
    }

    pub fn remove(&mut self, id: &str, path: &str) -> RefResult<bool> {
        let mut staged = self.live(id)?.clone();
        if !apply_path_remove(&mut staged, path) {
            return Ok(false);
        }
        self.commit_upsert(id, staged);
        Ok(true)
    }

    pub fn array_push(&mut self, id: &str, path: &str, value: Value) -> RefResult<bool> {
        let mut staged = self.live(id)?.clone();
        if !apply_path_push(&mut staged, path, value) {
            return Ok(false);
        }
        self.commit_upsert(id, staged);
        Ok(true)
    }

    /// Atomic multi-op patch. The whole batch resolves or nothing happens —
    /// and unlike the single ops there is no `Ok(false)` outcome.
    pub fn apply_ops(
        &mut self,
        id: &str,
        ops: &[RefOp],
        if_seq: Option<u64>,
    ) -> RefResult<(u64, Value)> {
        // Checked before the document lookup, matching `Database::apply_ops`.
        if ops.is_empty() {
            return Err(RefError::InvalidArgument);
        }
        let current = self.live(id)?.clone();
        if let Some(expected) = if_seq {
            let seq = self.doc_seq(id).unwrap_or(0);
            if expected != seq {
                return Err(RefError::Conflict { current_seq: seq });
            }
        }
        let mut staged = current;
        for op in ops {
            let applied = match op {
                RefOp::Set { path, value } => apply_path_set(&mut staged, path, value.clone()),
                RefOp::Remove { path } => apply_path_remove(&mut staged, path),
                RefOp::ArrayPush { path, value } => {
                    apply_path_push(&mut staged, path, value.clone())
                }
            };
            if !applied {
                return Err(RefError::InvalidArgument);
            }
        }
        let seq = self.commit_upsert(id, staged.clone());
        Ok((seq, staged))
    }

    pub fn delete(&mut self, id: &str) -> RefResult<u64> {
        let doc = self.live(id)?.clone();
        self.trash.push(doc);
        Ok(self.commit_delete(id))
    }

    /// Restore the most recently deleted copy of `id`. A live document is a
    /// caller error; a never-seen id is `InvalidArgument` too, because v2's
    /// `restore` refuses anything not currently in the deleted set.
    pub fn restore(&mut self, id: &str) -> RefResult<u64> {
        if !self.deleted.contains(id) {
            return Err(RefError::InvalidArgument);
        }
        let position = self
            .trash
            .iter()
            .rposition(|doc| doc.get("_id").and_then(Value::as_str) == Some(id))
            .expect("a deleted id always has a trash entry");
        let doc = self.trash[position].clone();
        self.deleted.remove(id);
        Ok(self.commit_upsert(id, doc))
    }

    // ── reads ───────────────────────────────────────────────────────────

    pub fn get(&self, id: &str) -> RefResult<Value> {
        self.live(id).cloned()
    }

    pub fn doc_seq(&self, id: &str) -> Option<u64> {
        self.doc_seqs.get(id).copied()
    }

    pub fn len(&self) -> usize {
        self.docs.len()
    }

    pub fn is_empty(&self) -> bool {
        self.docs.is_empty()
    }

    pub fn contains(&self, id: &str) -> bool {
        self.docs.contains_key(id)
    }

    /// Ids currently in the deleted set (restorable), ascending.
    pub fn deleted_ids(&self) -> Vec<String> {
        self.deleted.iter().cloned().collect()
    }

    /// Ids in scan order (id-ascending), with documents.
    pub fn iter_docs(&self) -> Vec<(String, Value)> {
        self.docs
            .iter()
            .map(|(id, doc)| (id.clone(), doc.clone()))
            .collect()
    }

    pub fn query_with(&self, ast: &Value, opts: &QueryOptions) -> RefResult<Vec<Value>> {
        validate_ast(ast)?;
        let mut rows: Vec<(String, Value)> = self
            .docs
            .iter()
            .filter(|(_, doc)| query_matches(doc, ast))
            .map(|(id, doc)| (id.clone(), doc.clone()))
            .collect();

        if let Some((field, dir)) = &opts.sort_by {
            rows.sort_by(|left, right| {
                let a = field_get(&left.1, field)
                    .map(ordered)
                    .unwrap_or(OrdValue::Null);
                let b = field_get(&right.1, field)
                    .map(ordered)
                    .unwrap_or(OrdValue::Null);
                let ordering = match dir {
                    SortDir::Asc => a.cmp(&b),
                    SortDir::Desc => a.cmp(&b).reverse(),
                };
                // v2 breaks ties by scan position; the scan is id-ascending.
                ordering.then_with(|| left.0.cmp(&right.0))
            });
        }

        let offset = opts.offset.unwrap_or(0);
        let limit = opts.limit.unwrap_or(usize::MAX);
        Ok(rows
            .into_iter()
            .skip(offset)
            .take(limit)
            .map(|(_, doc)| doc)
            .collect())
    }

    /// Term counts per field. Top-level fields only, strings only — exactly
    /// what both backends do (`doc.get(field)`, not `field_get`).
    pub fn facets(&self, fields: &[String]) -> Vec<(String, BTreeMap<String, usize>)> {
        let mut counts: Vec<(String, BTreeMap<String, usize>)> =
            fields.iter().map(|f| (f.clone(), BTreeMap::new())).collect();
        for doc in self.docs.values() {
            for (field, map) in counts.iter_mut() {
                let Some(value) = doc.get(field.as_str()) else {
                    continue;
                };
                let terms: Vec<&str> = match value {
                    Value::String(text) => vec![text.as_str()],
                    Value::Array(items) => items.iter().filter_map(Value::as_str).collect(),
                    _ => continue,
                };
                for term in terms {
                    *map.entry(term.to_string()).or_insert(0) += 1;
                }
            }
        }
        counts
    }

    pub fn lineage(&self) -> SourceLineage {
        SourceLineage {
            source_epoch_uuid: "reference-model".to_string(),
            last_seq: self.last_seq,
            min_available_seq: self.min_available_seq,
        }
    }

    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    pub fn snapshot_at(&self) -> Snapshot {
        Snapshot {
            lineage: self.lineage(),
            docs: self
                .docs
                .iter()
                .map(|(id, doc)| (id.clone(), doc.clone()))
                .collect(),
        }
    }

    pub fn changes_since(&self, since_seq: u64) -> RefResult<ChangeBatch> {
        let lineage = self.lineage();
        if since_seq > self.last_seq {
            return Err(RefError::CursorAhead {
                requested_seq: since_seq,
                last_seq: self.last_seq,
            });
        }
        if since_seq == self.last_seq {
            return Ok(ChangeBatch {
                lineage,
                events: Vec::new(),
            });
        }
        if since_seq.saturating_add(1) < self.min_available_seq {
            return Err(RefError::CursorExpired {
                requested_seq: since_seq,
                min_available_seq: self.min_available_seq,
                last_seq: self.last_seq,
            });
        }
        Ok(ChangeBatch {
            lineage,
            events: self
                .events
                .iter()
                .filter(|event| event.seq > since_seq)
                .cloned()
                .collect(),
        })
    }

    /// Every event this model has ever emitted, for history comparisons.
    pub fn all_events(&self) -> &[ChangeEvent] {
        &self.events
    }

    // ── internals ───────────────────────────────────────────────────────

    fn live(&self, id: &str) -> RefResult<&Value> {
        self.docs.get(id).ok_or(RefError::NotFound)
    }

    fn commit_upsert(&mut self, id: &str, doc: Value) -> u64 {
        self.last_seq += 1;
        let seq = self.last_seq;
        self.docs.insert(id.to_string(), doc.clone());
        self.doc_seqs.insert(id.to_string(), seq);
        self.deleted.remove(id);
        self.events.push(ChangeEvent {
            seq,
            id: id.to_string(),
            kind: ChangeKind::Upsert(doc),
        });
        seq
    }

    fn commit_delete(&mut self, id: &str) -> u64 {
        self.last_seq += 1;
        let seq = self.last_seq;
        self.docs.remove(id);
        self.doc_seqs.remove(id);
        self.deleted.insert(id.to_string());
        self.events.push(ChangeEvent {
            seq,
            id: id.to_string(),
            kind: ChangeKind::Delete,
        });
        seq
    }
}

/// A document builder shared by the oracle suites: a small, query-rich shape.
pub fn sample_doc(
    kind: &str,
    score: i64,
    rank: i64,
    title: &str,
    tags: &[&str],
    nested: i64,
) -> Value {
    let tags: Vec<Value> = tags
        .iter()
        .map(|t| Value::String((*t).to_string()))
        .collect();
    let mut map = Map::new();
    map.insert("kind".to_string(), Value::String(kind.to_string()));
    map.insert("score".to_string(), Value::from(score));
    map.insert("rank".to_string(), Value::from(rank));
    map.insert("title".to_string(), Value::String(title.to_string()));
    map.insert("tags".to_string(), Value::Array(tags));
    map.insert(
        "nested".to_string(),
        serde_json::json!({"k": nested, "deep": {"flag": nested % 2 == 0}}),
    );
    map.insert(
        "items".to_string(),
        Value::Array((0..nested.rem_euclid(4)).map(Value::from).collect()),
    );
    Value::Object(map)
}
