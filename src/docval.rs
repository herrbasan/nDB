//! Immutable, structurally shared document value tree for the RAM-first engine.
//!
//! [`DocVal`] is the internal representation behind the contract
//! (`docs/contract_ram-first-engine.md` §1.1). It is never exposed at the API
//! boundary: every public Rust API keeps `serde_json::Value`. Three invariants
//! make it usable as the authoritative state:
//!
//! 1. **Byte-identical JSON.** [`DocVal::write_json`] emits exactly what
//!    `serde_json::to_vec` emits for the equivalent `Value` — sorted object
//!    keys, identical string escaping, identical number formatting. The
//!    indexed-vs-scan oracle depends on it.
//! 2. **Structural sharing.** A mutation copies only the nodes on the changed
//!    path; untouched subtrees are shared by `Arc` refcount.
//! 3. **Delta-maintained size.** [`DocVal::approx_bytes`] is a pure sum over
//!    nodes, so a patch charges exactly `new − replaced + slot growth` and
//!    [`DocRecord::with_delta`] never walks a subtree.
//!
//! Query, comparison and index semantics mirror the v2 implementations in
//! `lib.rs` (`values_equal`, `value_cmp`, `query_matches`,
//! `evaluate_condition`, `field_get`, `index_keys`, `ordered_value`,
//! `project_doc`). Parity is verified by tests, not assumed.

use crate::{Error, OrderedValue, Result};
use serde_json::Value;
use std::cmp::Ordering;
use std::collections::BTreeMap;
use std::sync::Arc;

// ─── Memory model ────────────────────────────────────────────────────────────
//
// `approx_bytes` approximates the real heap footprint at ~1.3×: every node
// lives inside an `ArcInner` (two atomics + payload), arrays own a `Vec` of
// pointers, objects own a `BTreeMap` of boxed keys plus pointers, and strings
// own an `Arc<str>` buffer. Costs are per-node sums — never a walk of a shared
// subtree — so a delta is exact by construction.

const SCALE_NUM: usize = 13;
const SCALE_DEN: usize = 10;

const fn scaled(bytes: usize) -> usize {
    bytes * SCALE_NUM / SCALE_DEN
}

/// One `ArcInner<DocVal>`: strong + weak counts plus the enum payload.
const NODE_COST: usize = scaled(std::mem::size_of::<DocVal>() + 2 * std::mem::size_of::<usize>());
/// One element slot in an array's backing `Vec<Arc<DocVal>>`.
const SLOT_COST: usize = scaled(std::mem::size_of::<Arc<DocVal>>());
/// One `BTreeMap` entry: the boxed key pointer plus the value pointer.
const ENTRY_COST: usize = scaled(std::mem::size_of::<Box<str>>() + std::mem::size_of::<Arc<DocVal>>());

/// Cost of an object entry whose key is `key` long.
fn entry_cost(key: &str) -> usize {
    ENTRY_COST + scaled(key.len())
}

/// Cost of a 1-byte-per-char buffer plus its `Arc<str>` header, scaled.
fn string_cost(len: usize) -> usize {
    NODE_COST + scaled(len)
}

fn cost_delta(new: &DocVal, old: &DocVal) -> isize {
    (new.approx_bytes() as isize) - (old.approx_bytes() as isize)
}

// ─── Types ───────────────────────────────────────────────────────────────────

/// Immutable JSON value node. Subtrees are shared by `Arc`: a patch rebuilds
/// only the nodes between the root and the changed leaf.
#[derive(Debug, Clone)]
pub enum DocVal {
    /// JSON `null`.
    Null,
    /// JSON boolean.
    Bool(bool),
    /// JSON number, preserved exactly as `serde_json` received it (integer or
    /// float; formatting is `serde_json`'s).
    Number(serde_json::Number),
    /// JSON string.
    String(Arc<str>),
    /// JSON array.
    Array(Arc<Vec<Arc<DocVal>>>),
    /// JSON object, keyed by UTF-8 byte order — the same order
    /// `serde_json::Map` serialises in.
    Object(Arc<BTreeMap<Box<str>, Arc<DocVal>>>),
}

/// One published document version. Never mutated after publication: a change
/// produces a new `Arc<DocRecord>`, and the old one may live on in the change
/// ring or a pinned snapshot.
#[derive(Debug, Clone)]
pub struct DocRecord {
    /// Document id (`_id`).
    pub id: Arc<str>,
    /// Sequence that produced this record.
    pub doc_seq: u64,
    /// Published value tree.
    pub doc: Arc<DocVal>,
    /// Approximate live heap footprint of `doc` plus the id buffer.
    pub approx_bytes: usize,
}

impl DocRecord {
    /// Build a record, measuring the document tree once (construction only —
    /// never on a patch path).
    pub fn new(id: Arc<str>, doc_seq: u64, doc: Arc<DocVal>) -> Arc<DocRecord> {
        let approx_bytes = doc.approx_bytes() + scaled(id.len());
        Arc::new(DocRecord {
            id,
            doc_seq,
            doc,
            approx_bytes,
        })
    }

    /// Build the next version of `prev`, moving `approx_bytes` by `delta_bytes`
    /// without touching the tree.
    ///
    /// The delta is the same number the mutation returned, so
    /// `with_delta(..).approx_bytes == DocRecord::new(..).approx_bytes` for the
    /// resulting document — the invariant the tests assert.
    pub fn with_delta(
        prev: &DocRecord,
        new_seq: u64,
        new_doc: Arc<DocVal>,
        delta_bytes: isize,
    ) -> Arc<DocRecord> {
        let updated = prev.approx_bytes as i128 + delta_bytes as i128;
        assert!(
            updated >= 0,
            "approx_bytes underflow: prev={} delta={}",
            prev.approx_bytes,
            delta_bytes
        );
        let approx_bytes =
            usize::try_from(updated).expect("approx_bytes exceeds usize::MAX");
        Arc::new(DocRecord {
            id: Arc::clone(&prev.id),
            doc_seq: new_seq,
            doc: new_doc,
            approx_bytes,
        })
    }
}

// ─── Conversion & serialization ──────────────────────────────────────────────

impl DocVal {
    /// Build a tree from a `serde_json` value.
    pub fn from_value(value: &Value) -> Arc<DocVal> {
        Arc::new(match value {
            Value::Null => DocVal::Null,
            Value::Bool(flag) => DocVal::Bool(*flag),
            Value::Number(number) => DocVal::Number(number.clone()),
            Value::String(text) => DocVal::String(Arc::from(text.as_str())),
            Value::Array(items) => DocVal::Array(Arc::new(
                items.iter().map(|item| DocVal::from_value(item)).collect(),
            )),
            Value::Object(map) => DocVal::Object(Arc::new(
                map.iter()
                    .map(|(key, item)| (Box::from(key.as_str()), DocVal::from_value(item)))
                    .collect(),
            )),
        })
    }

    /// Materialise a `serde_json` value (boundary conversion only).
    pub fn to_value(&self) -> Value {
        match self {
            DocVal::Null => Value::Null,
            DocVal::Bool(flag) => Value::Bool(*flag),
            DocVal::Number(number) => Value::Number(number.clone()),
            DocVal::String(text) => Value::String(text.to_string()),
            DocVal::Array(items) => {
                Value::Array(items.iter().map(|item| item.to_value()).collect())
            }
            DocVal::Object(map) => Value::Object(
                map.iter()
                    .map(|(key, item)| (key.to_string(), item.to_value()))
                    .collect(),
            ),
        }
    }

    /// Append the exact `serde_json` serialization of this value.
    pub fn write_json(&self, out: &mut Vec<u8>) {
        match self {
            DocVal::Null => out.extend_from_slice(b"null"),
            DocVal::Bool(true) => out.extend_from_slice(b"true"),
            DocVal::Bool(false) => out.extend_from_slice(b"false"),
            // `serde_json::Number`'s `Display` is the same formatting its
            // serializer uses (itoa for integers, ryu for floats).
            DocVal::Number(number) => out.extend_from_slice(number.to_string().as_bytes()),
            DocVal::String(text) => write_json_str(text, out),
            DocVal::Array(items) => {
                out.push(b'[');
                for (index, item) in items.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    item.write_json(out);
                }
                out.push(b']');
            }
            DocVal::Object(map) => {
                out.push(b'{');
                for (index, (key, item)) in map.iter().enumerate() {
                    if index > 0 {
                        out.push(b',');
                    }
                    write_json_str(key, out);
                    out.push(b':');
                    item.write_json(out);
                }
                out.push(b'}');
            }
        }
    }

    /// The exact `serde_json` serialization of this value as text.
    pub fn to_json_string(&self) -> String {
        let mut out = Vec::new();
        self.write_json(&mut out);
        String::from_utf8(out).expect("JSON output is valid UTF-8")
    }

    /// Approximate live heap footprint of this node and everything it owns.
    ///
    /// A pure sum over nodes, so `delta == after − before` for every patch.
    pub fn approx_bytes(&self) -> usize {
        match self {
            DocVal::Null | DocVal::Bool(_) | DocVal::Number(_) => NODE_COST,
            DocVal::String(text) => string_cost(text.len()),
            DocVal::Array(items) => {
                NODE_COST
                    + items.len() * SLOT_COST
                    + items.iter().map(|item| item.approx_bytes()).sum::<usize>()
            }
            DocVal::Object(map) => {
                NODE_COST
                    + map
                        .iter()
                        .map(|(key, item)| entry_cost(key) + item.approx_bytes())
                        .sum::<usize>()
            }
        }
    }
}

/// serde_json escapes exactly `"`, `\` and the C0 range (with `\b \t \n \f \r`
/// shortcuts); every other byte, including all multi-byte UTF-8, is emitted
/// verbatim.
fn write_json_str(text: &str, out: &mut Vec<u8>) {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    out.push(b'"');
    for &byte in text.as_bytes() {
        match byte {
            b'"' => out.extend_from_slice(b"\\\""),
            b'\\' => out.extend_from_slice(b"\\\\"),
            0x08 => out.extend_from_slice(b"\\b"),
            0x09 => out.extend_from_slice(b"\\t"),
            0x0a => out.extend_from_slice(b"\\n"),
            0x0c => out.extend_from_slice(b"\\f"),
            0x0d => out.extend_from_slice(b"\\r"),
            0x00..=0x1f => {
                out.extend_from_slice(b"\\u00");
                out.push(HEX[(byte >> 4) as usize]);
                out.push(HEX[(byte & 0x0f) as usize]);
            }
            _ => out.push(byte),
        }
    }
    out.push(b'"');
}

// ─── Path resolution ─────────────────────────────────────────────────────────

impl DocVal {
    /// Resolve a dot path, array-aware: a numeric segment indexes an array
    /// (mirrors `field_get` in `lib.rs`).
    pub fn get_path<'a>(&'a self, path: &str) -> Option<&'a DocVal> {
        let mut current = self;
        for part in path.split('.') {
            current = match current {
                DocVal::Array(items) => items.get(part.parse::<usize>().ok()?)?.as_ref(),
                DocVal::Object(map) => map.get(part)?.as_ref(),
                _ => return None,
            };
        }
        Some(current)
    }
}

fn parse_path(path: &str) -> Result<Vec<&str>> {
    if path.is_empty() {
        return Err(Error::invalid_arg("patch path must not be empty"));
    }
    let segments: Vec<&str> = path.split('.').collect();
    if segments.iter().any(|segment| segment.is_empty()) {
        return Err(Error::invalid_arg(format!(
            "patch path '{path}' contains an empty segment"
        )));
    }
    Ok(segments)
}

// ─── Path-copying mutations ──────────────────────────────────────────────────

/// Assign `value` at a dot path, copying only the nodes on that path.
///
/// Returns `(new_root, applied, delta_bytes)`. `applied == false` means the
/// path did not resolve (`delta_bytes == 0`, `new_root` is the input): the
/// same no-op contract as `apply_path_set` in `lib.rs`.
pub fn set_path(
    root: &Arc<DocVal>,
    path: &str,
    value: Arc<DocVal>,
) -> Result<(Arc<DocVal>, bool, isize)> {
    let segments = parse_path(path)?;
    match set_in(root, &segments, 0, &value) {
        Some((next, delta)) => Ok((next, true, delta)),
        None => Ok((Arc::clone(root), false, 0)),
    }
}

/// Remove the value at a dot path. `applied == false` is a no-op.
pub fn remove_path(root: &Arc<DocVal>, path: &str) -> Result<(Arc<DocVal>, bool, isize)> {
    let segments = parse_path(path)?;
    match remove_in(root, &segments, 0) {
        Some((next, delta)) => Ok((next, true, delta)),
        None => Ok((Arc::clone(root), false, 0)),
    }
}

/// Push `value` onto the array at a dot path. Missing final object key is
/// created as a one-element array (mirrors `apply_path_push`).
pub fn array_push_path(
    root: &Arc<DocVal>,
    path: &str,
    value: Arc<DocVal>,
) -> Result<(Arc<DocVal>, bool, isize)> {
    let segments = parse_path(path)?;
    match push_in(root, &segments, 0, &value) {
        Some((next, delta)) => Ok((next, true, delta)),
        None => Ok((Arc::clone(root), false, 0)),
    }
}

fn replace_at(slot: usize, replacement: Arc<DocVal>, items: &[Arc<DocVal>]) -> Arc<DocVal> {
    let mut next: Vec<Arc<DocVal>> = Vec::with_capacity(items.len());
    next.extend_from_slice(items);
    next[slot] = replacement;
    Arc::new(DocVal::Array(Arc::new(next)))
}

// One container level is rebuilt on the way to the leaf: an object's own keys
// are copied (a `BTreeMap` cannot share them), its child values are not.
fn with_key(map: &BTreeMap<Box<str>, Arc<DocVal>>, key: &str, value: Arc<DocVal>) -> Arc<DocVal> {
    let mut next = map.clone();
    next.insert(Box::from(key), value);
    Arc::new(DocVal::Object(Arc::new(next)))
}

fn set_in(
    node: &Arc<DocVal>,
    segments: &[&str],
    depth: usize,
    value: &Arc<DocVal>,
) -> Option<(Arc<DocVal>, isize)> {
    if depth >= segments.len() {
        return None;
    }
    let key = segments[depth];
    let is_last = depth + 1 == segments.len();

    if let Ok(index) = key.parse::<usize>() {
        // Mirror `walk_and_set`: a numeric segment only ever addresses an
        // array. An object that happens to hold "2" is not reachable here.
        if let DocVal::Array(items) = node.as_ref() {
            if index < items.len() {
                if is_last {
                    let delta = cost_delta(value, &items[index]);
                    return Some((replace_at(index, Arc::clone(value), items), delta));
                }
                return set_in(&items[index], segments, depth + 1, value).map(|(child, delta)| {
                    (replace_at(index, child, items), delta)
                });
            }
        }
        return None;
    }

    if let DocVal::Object(map) = node.as_ref() {
        if is_last {
            let delta = match map.get(key) {
                Some(old) => cost_delta(value, old),
                None => (entry_cost(key) + value.approx_bytes()) as isize,
            };
            return Some((with_key(map, key, Arc::clone(value)), delta));
        }
        if let Some(child) = map.get(key) {
            return set_in(child, segments, depth + 1, value)
                .map(|(next, delta)| (with_key(map, key, next), delta));
        }
        return None;
    }
    None
}

fn remove_in(
    node: &Arc<DocVal>,
    segments: &[&str],
    depth: usize,
) -> Option<(Arc<DocVal>, isize)> {
    if depth >= segments.len() {
        return None;
    }
    let key = segments[depth];
    let is_last = depth + 1 == segments.len();

    if let Ok(index) = key.parse::<usize>() {
        if let DocVal::Array(items) = node.as_ref() {
            if index < items.len() {
                if is_last {
                    let delta = -((items[index].approx_bytes() + SLOT_COST) as isize);
                    let mut next: Vec<Arc<DocVal>> = Vec::with_capacity(items.len() - 1);
                    next.extend_from_slice(&items[..index]);
                    next.extend_from_slice(&items[index + 1..]);
                    return Some((Arc::new(DocVal::Array(Arc::new(next))), delta));
                }
                return remove_in(&items[index], segments, depth + 1).map(|(child, delta)| {
                    (replace_at(index, child, items), delta)
                });
            }
        }
        return None;
    }

    if let DocVal::Object(map) = node.as_ref() {
        if is_last {
            let removed = map.get(key)?;
            let delta = -((removed.approx_bytes() + entry_cost(key)) as isize);
            let mut next = map.as_ref().clone();
            next.remove(key);
            return Some((Arc::new(DocVal::Object(Arc::new(next))), delta));
        }
        if let Some(child) = map.get(key) {
            return remove_in(child, segments, depth + 1)
                .map(|(next, delta)| (with_key(map, key, next), delta));
        }
        return None;
    }
    None
}

fn push_in(
    node: &Arc<DocVal>,
    segments: &[&str],
    depth: usize,
    value: &Arc<DocVal>,
) -> Option<(Arc<DocVal>, isize)> {
    if depth >= segments.len() {
        return None;
    }
    let key = segments[depth];
    let is_last = depth + 1 == segments.len();

    if let Ok(index) = key.parse::<usize>() {
        if let DocVal::Array(items) = node.as_ref() {
            if index < items.len() {
                if is_last {
                    let DocVal::Array(inner) = items[index].as_ref() else {
                        return None;
                    };
                    let mut next_inner: Vec<Arc<DocVal>> = Vec::with_capacity(inner.len() + 1);
                    next_inner.extend_from_slice(inner);
                    next_inner.push(Arc::clone(value));
                    let delta = (value.approx_bytes() + SLOT_COST) as isize;
                    return Some((
                        replace_at(
                            index,
                            Arc::new(DocVal::Array(Arc::new(next_inner))),
                            items,
                        ),
                        delta,
                    ));
                }
                return push_in(&items[index], segments, depth + 1, value).map(|(child, delta)| {
                    (replace_at(index, child, items), delta)
                });
            }
        }
        return None;
    }

    if let DocVal::Object(map) = node.as_ref() {
        if is_last {
            return match map.get(key) {
                Some(slot) => {
                    let DocVal::Array(inner) = slot.as_ref() else {
                        return None;
                    };
                    let mut next_inner: Vec<Arc<DocVal>> = Vec::with_capacity(inner.len() + 1);
                    next_inner.extend_from_slice(inner);
                    next_inner.push(Arc::clone(value));
                    let delta = (value.approx_bytes() + SLOT_COST) as isize;
                    Some((
                        with_key(
                            map,
                            key,
                            Arc::new(DocVal::Array(Arc::new(next_inner))),
                        ),
                        delta,
                    ))
                }
                None => {
                    let created = Arc::new(DocVal::Array(Arc::new(vec![Arc::clone(value)])));
                    let delta =
                        (entry_cost(key) + value.approx_bytes() + SLOT_COST + NODE_COST) as isize;
                    Some((with_key(map, key, created), delta))
                }
            };
        }
        if let Some(child) = map.get(key) {
            return push_in(child, segments, depth + 1, value)
                .map(|(next, delta)| (with_key(map, key, next), delta));
        }
        return None;
    }
    None
}

// ─── Comparison (parity with values_equal / value_cmp) ──────────────────────

/// Equality with `lib.rs`'s `values_equal` semantics — including its two-level
/// rule: numbers compare as `f64` **only where `values_equal` compares them
/// directly**, while nested numbers keep `serde_json`'s exact equality
/// (`1` and `1.0` are different inside an object).
pub fn eq_docval(a: &DocVal, b: &DocVal) -> bool {
    match (a, b) {
        (DocVal::Number(left), DocVal::Number(right)) => left.as_f64() == right.as_f64(),
        _ => eq_docval_exact(a, b),
    }
}

fn eq_docval_exact(a: &DocVal, b: &DocVal) -> bool {
    match (a, b) {
        (DocVal::Null, DocVal::Null) => true,
        (DocVal::Bool(left), DocVal::Bool(right)) => left == right,
        (DocVal::Number(left), DocVal::Number(right)) => left == right,
        (DocVal::String(left), DocVal::String(right)) => left == right,
        (DocVal::Array(left), DocVal::Array(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right.iter())
                    .all(|(l, r)| eq_docval_exact(l, r))
        }
        (DocVal::Object(left), DocVal::Object(right)) => {
            left.len() == right.len()
                && left.iter().all(|(key, l)| {
                    right
                        .get(key.as_ref())
                        .is_some_and(|r| eq_docval_exact(l, r))
                })
        }
        _ => false,
    }
}

/// Equality against a `serde_json` operand with `values_equal` semantics.
pub fn eq_json_value(a: &DocVal, b: &Value) -> bool {
    match (a, b) {
        (DocVal::Number(left), Value::Number(right)) => left.as_f64() == right.as_f64(),
        _ => eq_json_exact(a, b),
    }
}

fn eq_json_exact(a: &DocVal, b: &Value) -> bool {
    match (a, b) {
        (DocVal::Null, Value::Null) => true,
        (DocVal::Bool(left), Value::Bool(right)) => left == right,
        (DocVal::Number(left), Value::Number(right)) => left == right,
        (DocVal::String(left), Value::String(right)) => left.as_ref() == right.as_str(),
        (DocVal::Array(items), Value::Array(values)) => {
            items.len() == values.len()
                && items
                    .iter()
                    .zip(values.iter())
                    .all(|(item, value)| eq_json_exact(item, value))
        }
        (DocVal::Object(map), Value::Object(values)) => {
            map.len() == values.len()
                && map.iter().all(|(key, item)| {
                    values
                        .get(key.as_ref())
                        .is_some_and(|value| eq_json_exact(item, value))
                })
        }
        _ => false,
    }
}

impl DocVal {
    /// Canonical ordered key, identical to `lib.rs`'s `ordered_value`.
    pub(crate) fn ordered_key(&self) -> OrderedValue {
        match self {
            DocVal::Null => OrderedValue::Null,
            DocVal::Bool(flag) => OrderedValue::Bool(*flag),
            DocVal::Number(number) => OrderedValue::Number(crate::OrderedF64(
                number
                    .as_f64()
                    .expect("serde_json number is not representable as f64"),
            )),
            DocVal::String(text) => OrderedValue::String(text.to_string()),
            other => OrderedValue::Composite(other.to_json_string()),
        }
    }
}

/// Total order matching `lib.rs`'s `value_cmp`.
pub fn cmp_docval(a: &DocVal, b: &DocVal) -> Ordering {
    a.ordered_key().cmp(&b.ordered_key())
}

fn cmp_json(a: &DocVal, b: &Value) -> Ordering {
    a.ordered_key().cmp(&crate::ordered_value(b))
}

// ─── Index keys (parity with index_keys / ordered_value) ────────────────────

/// The keys one indexed field value contributes: an array contributes itself
/// **and** each element, exactly as `index_keys` in `lib.rs` does.
// Wired up with the RAM-first index maintenance in the next slice; the parity
// tests already exercise it.
#[allow(dead_code)]
pub(crate) fn ordered_index_keys(value: &DocVal) -> Vec<OrderedValue> {
    match value {
        DocVal::Array(items) if !items.is_empty() => {
            let mut keys = Vec::with_capacity(items.len() + 1);
            keys.push(value.ordered_key());
            keys.extend(items.iter().map(|item| item.ordered_key()));
            keys
        }
        _ => vec![value.ordered_key()],
    }
}

impl DocVal {
    /// Index keys contributed by `field` on this document. An unresolved field
    /// contributes nothing (it is not indexed at all).
    // Wired up with the RAM-first index maintenance in the next slice.
    #[allow(dead_code)]
    pub(crate) fn extract_ordered_keys(&self, field: &str) -> Vec<OrderedValue> {
        match self.get_path(field) {
            Some(value) => ordered_index_keys(value),
            None => Vec::new(),
        }
    }
}

// ─── Filter AST (parity with query_matches / evaluate_condition / field_get) ─

impl DocVal {
    /// Evaluate a Layer-3 query AST against this document, exactly as
    /// `query_matches` does.
    pub fn matches_filter(&self, ast: &Value) -> bool {
        query_matches_inner(self, ast)
    }
}

fn query_matches_inner(doc: &DocVal, ast: &Value) -> bool {
    match ast {
        Value::Object(map) => {
            // Combinators short-circuit the whole object — a stray sibling
            // field key next to `$and` is ignored, matching `query_matches`.
            if let Some(and_expr) = map.get("$and") {
                return and_expr
                    .as_array()
                    .map(|arr| arr.iter().all(|cond| query_matches_inner(doc, cond)))
                    .unwrap_or(false);
            }
            if let Some(or_expr) = map.get("$or") {
                return or_expr
                    .as_array()
                    .map(|arr| arr.iter().any(|cond| query_matches_inner(doc, cond)))
                    .unwrap_or(false);
            }
            if let Some(not_expr) = map.get("$not") {
                return !query_matches_inner(doc, not_expr);
            }

            map.iter().all(|(field, condition)| match doc.get_path(field) {
                None => {
                    // Only `$exists: false` matches a missing field; every
                    // other condition on a missing field is false.
                    if let Value::Object(op_map) = condition {
                        if let Some(exists_val) = op_map.get("$exists") {
                            return !exists_val.as_bool().unwrap_or(true);
                        }
                    }
                    false
                }
                Some(field_val) => evaluate_condition(field_val, condition),
            })
        }
        // Array at top level = implicit $and.
        Value::Array(conditions) => conditions.iter().all(|cond| query_matches_inner(doc, cond)),
        _ => false,
    }
}

fn evaluate_condition(field_val: &DocVal, condition: &Value) -> bool {
    match condition {
        Value::Object(op_map) => op_map.iter().all(|(op, operand)| match op.as_str() {
            "$eq" => eq_json_value(field_val, operand),
            "$ne" => !eq_json_value(field_val, operand),
            "$gt" => cmp_json(field_val, operand) == Ordering::Greater,
            "$gte" => cmp_json(field_val, operand) != Ordering::Less,
            "$lt" => cmp_json(field_val, operand) == Ordering::Less,
            "$lte" => cmp_json(field_val, operand) != Ordering::Greater,
            "$in" => operand
                .as_array()
                .map(|arr| {
                    if let DocVal::Array(field_arr) = field_val {
                        field_arr
                            .iter()
                            .any(|el| arr.iter().any(|v| eq_json_value(el, v)))
                    } else {
                        arr.iter().any(|v| eq_json_value(field_val, v))
                    }
                })
                .unwrap_or(false),
            "$nin" => operand
                .as_array()
                .map(|arr| {
                    let hit = if let DocVal::Array(field_arr) = field_val {
                        field_arr
                            .iter()
                            .any(|el| arr.iter().any(|v| eq_json_value(el, v)))
                    } else {
                        arr.iter().any(|v| eq_json_value(field_val, v))
                    };
                    !hit
                })
                .unwrap_or(true),
            "$exists" => operand.as_bool().unwrap_or(true),
            // Unvalidated AST only; `validate_query_ast` rejects unknowns on
            // the HTTP path. Mirrors `evaluate_condition`.
            _ => true,
        }),
        // Implicit $eq.
        _ => eq_json_value(field_val, condition),
    }
}

// ─── Projection (parity with project_doc) ───────────────────────────────────

impl DocVal {
    /// Serialize the `fields` projection directly, without materialising a
    /// `serde_json::Value`. Byte-identical to
    /// `serde_json::to_vec(&project_doc(&doc.to_value(), fields))`.
    pub fn project_write_json(&self, fields: &[String], out: &mut Vec<u8>) {
        let mut entries: BTreeMap<&str, &DocVal> = BTreeMap::new();
        for field in fields {
            if let Some(value) = self.get_path(field) {
                entries.insert(field.as_str(), value);
            }
        }
        if let Some(id) = self.get_path("_id") {
            entries.insert("_id", id);
        }
        out.push(b'{');
        for (index, (key, value)) in entries.iter().enumerate() {
            if index > 0 {
                out.push(b',');
            }
            write_json_str(key, out);
            out.push(b':');
            value.write_json(out);
        }
        out.push(b'}');
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        apply_path_push, apply_path_remove, apply_path_set, field_get, index_keys, project_doc,
        query_matches, value_cmp, values_equal,
    };
    use serde_json::{json, Number};

    fn build(value: &Value) -> Arc<DocVal> {
        DocVal::from_value(value)
    }

    fn tricky_corpus() -> Vec<Value> {
        vec![
            Value::Null,
            json!(true),
            json!(false),
            json!(""),
            json!("\u{0}\u{1}\u{1f}\u{7f}"),
            json!("quote \" backslash \\ slash / tab \t nl \n cr \r ff \u{c} bs \u{8}"),
            json!("unicode: äöü 中文 🚀 \u{2028}\u{2029}"),
            json!("é"),
            json!(0),
            json!(-1),
            json!(i64::MIN),
            json!(i64::MAX),
            json!(u64::MAX),
            json!(serde_json::Number::from_f64(-0.0).unwrap()),
            json!(serde_json::Number::from_f64(0.0).unwrap()),
            json!(serde_json::Number::from_f64(1.0).unwrap()),
            json!(serde_json::Number::from_f64(0.1).unwrap()),
            json!(serde_json::Number::from_f64(-1.5e300).unwrap()),
            json!(serde_json::Number::from_f64(5e-324).unwrap()),
            json!([]),
            json!({}),
            json!([1, [2, [3, {"k": "v"}]], {"z": 1, "a": [true, null]}]),
            json!({"z": 1, "a": 2, "m": {"b": [1, 2, 3], "a": "x"}, "empty": {}, "n": null}),
            json!({"deep": {"deeper": {"deepest": ["x", {"y": [1.0, 2]}]}}}),
        ]
    }

    #[test]
    fn json_serialization_is_byte_identical() {
        for value in tricky_corpus() {
            let expected = serde_json::to_vec(&value).expect("serialize corpus value");
            let doc = build(&value);
            let mut written = Vec::new();
            doc.write_json(&mut written);
            assert_eq!(
                written,
                expected,
                "write_json diverged for {value:?}:\n got {}\nwant {}",
                String::from_utf8_lossy(&written),
                String::from_utf8_lossy(&expected)
            );
            assert_eq!(doc.to_json_string().as_bytes(), expected.as_slice());
            assert_eq!(doc.to_value(), value);
        }
    }

    #[test]
    fn number_formatting_matches_serializer() {
        let numbers = [
            Number::from(0u64),
            Number::from(u64::MAX),
            Number::from(i64::MIN),
            Number::from(i64::MAX),
            Number::from_f64(-0.0).unwrap(),
            Number::from_f64(0.1 + 0.2).unwrap(),
            Number::from_f64(1e-7).unwrap(),
            Number::from_f64(1e21).unwrap(),
            Number::from_f64(f64::MAX).unwrap(),
            Number::from_f64(f64::MIN_POSITIVE).unwrap(),
        ];
        for number in numbers {
            let value = Value::Number(number.clone());
            assert_eq!(
                number.to_string(),
                serde_json::to_string(&value).unwrap(),
                "number Display diverged"
            );
            let doc = DocVal::Number(number);
            assert_eq!(doc.to_json_string(), serde_json::to_string(&value).unwrap());
        }
    }

    #[test]
    fn approx_bytes_is_additive_and_monotonic() {
        let doc = build(&json!({"a": [1, 2, 3], "b": "hello", "c": {"d": null}}));
        let total = doc.approx_bytes();
        assert!(total > 0);
        let bigger = build(&json!({"a": [1, 2, 3], "b": "hello", "c": {"d": null}, "e": "x"}));
        assert!(bigger.approx_bytes() > total);
        // Sum over children equals the parent cost minus the node itself.
        if let DocVal::Object(map) = doc.as_ref() {
            let children: usize = map
                .iter()
                .map(|(key, item)| entry_cost(key) + item.approx_bytes())
                .sum();
            assert_eq!(total, NODE_COST + children);
        } else {
            panic!("expected object root");
        }
    }

    #[test]
    fn malformed_paths_fail_loud() {
        let doc = build(&json!({"a": 1}));
        assert!(set_path(&doc, "", build(&json!(2))).is_err());
        assert!(set_path(&doc, "a..b", build(&json!(2))).is_err());
        assert!(remove_path(&doc, "a.").is_err());
        assert!(array_push_path(&doc, ".a", build(&json!(2))).is_err());
    }

    #[test]
    fn no_op_mutations_report_not_applied() {
        let doc = build(&json!({"a": 1, "arr": [1, 2]}));
        assert!(!set_path(&doc, "a.b", build(&json!(1))).unwrap().1);
        assert!(!set_path(&doc, "arr.9", build(&json!(1))).unwrap().1);
        assert!(!set_path(&doc, "0", build(&json!(1))).unwrap().1);
        assert!(!remove_path(&doc, "missing").unwrap().1);
        assert!(!remove_path(&doc, "arr.9").unwrap().1);
        assert!(!array_push_path(&doc, "a", build(&json!(1))).unwrap().1);
        assert!(!array_push_path(&doc, "arr.1", build(&json!(1))).unwrap().1);
        for (_, applied, delta) in [
            set_path(&doc, "arr.5.x", build(&json!(1))).unwrap(),
            remove_path(&doc, "missing").unwrap(),
            array_push_path(&doc, "a", build(&json!(1))).unwrap(),
        ] {
            assert!(!applied);
            assert_eq!(delta, 0);
        }
        // `set` creates a missing final key — exactly like `apply_path_set`.
        let (next, applied, delta) = set_path(&doc, "missing", build(&json!(1))).unwrap();
        assert!(applied);
        assert_eq!(next.to_value(), json!({"a": 1, "arr": [1, 2], "missing": 1}));
        assert_eq!(delta, next.approx_bytes() as isize - doc.approx_bytes() as isize);

        // A numeric-looking key never addresses an object field (walk_and_set
        // only tries the array arm for a numeric segment).
        let numeric = build(&json!({"2": "x", "list": []}));
        assert!(!set_path(&numeric, "2", build(&json!(1))).unwrap().1);
        assert!(!remove_path(&numeric, "2").unwrap().1);
        assert!(!array_push_path(&numeric, "2", build(&json!(1))).unwrap().1);
        assert_eq!(
            set_path(&numeric, "2", build(&json!(1))).unwrap().0.to_value(),
            json!({"2": "x", "list": []})
        );
    }

    #[test]
    fn idempotent_set_still_applies() {
        let doc = build(&json!({"a": 1}));
        let value = build(&json!(1));
        let (next, applied, delta) = set_path(&doc, "a", Arc::clone(&value)).unwrap();
        assert!(applied);
        assert_eq!(delta, 0);
        assert_eq!(next.to_value(), json!({"a": 1}));
        // A re-assignment publishes a new root even when nothing changed.
        assert!(!Arc::ptr_eq(&doc, &next));
    }

    #[test]
    fn dot_notation_and_array_segments() {
        let doc = build(&json!({"a": {"b": [{"c": 1}, {"c": 2}]}}));
        let (next, applied, _) = set_path(&doc, "a.b.1.c", build(&json!(9))).unwrap();
        assert!(applied);
        assert_eq!(next.to_value(), json!({"a": {"b": [{"c": 1}, {"c": 9}]}}));
        let (next, applied, _) = remove_path(&next, "a.b.0").unwrap();
        assert!(applied);
        assert_eq!(next.to_value(), json!({"a": {"b": [{"c": 9}]}}));
        let (next, applied, _) = array_push_path(&next, "a.b", build(&json!({"c": 3}))).unwrap();
        assert!(applied);
        assert_eq!(next.to_value(), json!({"a": {"b": [{"c": 9}, {"c": 3}]}}));
        let (next, applied, _) = array_push_path(&next, "a.created", build(&json!("x"))).unwrap();
        assert!(applied);
        assert_eq!(
            next.to_value(),
            json!({"a": {"b": [{"c": 9}, {"c": 3}], "created": ["x"]}})
        );
        assert_eq!(
            next.get_path("a.created.0").map(|v| v.to_value()),
            Some(json!("x"))
        );
        assert_eq!(next.get_path("a.b.1.c").map(|v| v.to_value()), Some(json!(3)));
        assert!(next.get_path("a.b.5").is_none());
        assert!(next.get_path("a.b.x").is_none());
    }

    #[test]
    fn nested_number_equality_follows_serde_json() {
        // `values_equal` compares numbers as f64 only at the top level; inside
        // containers `serde_json` exactness applies.
        let int_doc = build(&json!({"n": 1}));
        let float_doc = build(&json!({"n": 1.0}));
        assert_eq!(
            eq_docval(&int_doc, &float_doc),
            values_equal(&json!({"n": 1}), &json!({"n": 1.0}))
        );
        assert!(!eq_docval(&int_doc, &float_doc));
        assert!(eq_docval(&build(&json!(1)), &build(&json!(1.0))));
        assert!(eq_json_value(&build(&json!(1)), &json!(1.0)));
        assert!(!eq_json_value(&int_doc, &json!({"n": 1.0})));
        // u64 values that collapse to the same f64 compare equal, as in lib.rs.
        let a = build(&json!(9_007_199_254_740_993u64));
        let b = build(&json!(9_007_199_254_740_992u64));
        assert_eq!(
            eq_docval(&a, &b),
            values_equal(&json!(9_007_199_254_740_993u64), &json!(9_007_199_254_740_992u64))
        );
    }

    #[test]
    fn ordered_keys_match_lib_rs() {
        let cases = [
            json!({"f": [1, 2], "g": "x"}),
            json!({"f": []}),
            json!({"f": {"a": 1}}),
            json!({"f": null}),
            json!({"f": true}),
            json!({"f": [{"n": 1}, "s"]}),
            json!({"f": {"b": 2, "a": 1}, "g": -0.0}),
        ];
        for value in cases {
            let doc = build(&value);
            for field in ["f", "g", "missing", "f.0"] {
                let expected: Vec<OrderedValue> = field_get(&value, field)
                    .map(index_keys)
                    .unwrap_or_default();
                assert_eq!(doc.extract_ordered_keys(field), expected, "field {field}");
            }
        }
    }

    // ── Randomized parity ───────────────────────────────────────────────────

    const KEYS: [&str; 6] = ["a", "b", "c", "items", "meta", "n"];

    fn random_string() -> String {
        let pool = [
            "alpha", "", "with space", "quote\"inside", "back\\slash", "äöü", "中文", "🚀",
            "line\nbreak", "tab\there", "ctrl\u{1}", "mixed \"ä\" \\ 🚀",
        ];
        let mut text = String::new();
        for _ in 0..fastrand::usize(1..=2) {
            text.push_str(pool[fastrand::usize(0..pool.len())]);
        }
        text
    }

    fn random_value(depth: usize) -> Value {
        match fastrand::u8(0..=9) {
            0 => Value::Null,
            1 => Value::Bool(fastrand::bool()),
            2 => Value::Number(Number::from(fastrand::i64(-1_000..=1_000))),
            3 => Value::Number(Number::from(fastrand::u64(..))),
            4 => Value::Number(
                Number::from_f64(fastrand::f64() * 1_000.0 - 500.0).expect("finite"),
            ),
            5 => Value::String(random_string()),
            6 | 7 if depth < 3 => {
                let len = fastrand::usize(0..=3);
                Value::Array((0..len).map(|_| random_value(depth + 1)).collect())
            }
            8 | 9 if depth < 3 => {
                let len = fastrand::usize(0..=3);
                let mut map = serde_json::Map::new();
                for _ in 0..len {
                    map.insert(
                        KEYS[fastrand::usize(0..KEYS.len())].to_string(),
                        random_value(depth + 1),
                    );
                }
                Value::Object(map)
            }
            _ => Value::Number(Number::from(0u64)),
        }
    }

    fn collect_paths(value: &Value, prefix: &str, depth: usize, out: &mut Vec<String>) {
        if depth > 2 {
            return;
        }
        match value {
            Value::Object(map) => {
                for (key, child) in map {
                    let path = if prefix.is_empty() {
                        key.clone()
                    } else {
                        format!("{prefix}.{key}")
                    };
                    out.push(path.clone());
                    collect_paths(child, &path, depth + 1, out);
                }
            }
            Value::Array(items) => {
                for (index, child) in items.iter().enumerate() {
                    let path = if prefix.is_empty() {
                        index.to_string()
                    } else {
                        format!("{prefix}.{index}")
                    };
                    out.push(path.clone());
                    collect_paths(child, &path, depth + 1, out);
                }
            }
            _ => {}
        }
    }

    fn candidate_paths(value: &Value) -> Vec<String> {
        let mut paths = Vec::new();
        collect_paths(value, "", 0, &mut paths);
        // Deliberately unresolvable shapes.
        paths.push("a.b.c.d".to_string());
        paths.push("missing".to_string());
        paths.push("items.99".to_string());
        paths.push("meta.0".to_string());
        paths.push("n.deeper".to_string());
        paths.push("items.x".to_string());
        if paths.is_empty() {
            paths.push("a".to_string());
        }
        paths
    }

    fn mutation_parity(iterations: usize) {
        fastrand::seed(0x5EED_1234);
        for _ in 0..iterations {
            let value = random_value(0);
            let doc = build(&value);
            let paths = candidate_paths(&value);
            let path = paths[fastrand::usize(0..paths.len())].clone();
            let new_value = random_value(0);
            let new_doc = build(&new_value);

            let mut expected = value.clone();
            let expected_applied = apply_path_set(&mut expected, &path, new_value.clone());
            let (next, applied, delta) = set_path(&doc, &path, Arc::clone(&new_doc)).unwrap();
            assert_eq!(applied, expected_applied, "set applied on {path}");
            assert_eq!(next.to_value(), expected, "set result on {path}");
            assert_eq!(
                delta,
                next.approx_bytes() as isize - doc.approx_bytes() as isize,
                "set delta on {path}"
            );

            let mut expected = value.clone();
            let expected_applied = apply_path_remove(&mut expected, &path);
            let (next, applied, delta) = remove_path(&doc, &path).unwrap();
            assert_eq!(applied, expected_applied, "remove applied on {path}");
            assert_eq!(next.to_value(), expected, "remove result on {path}");
            assert_eq!(
                delta,
                next.approx_bytes() as isize - doc.approx_bytes() as isize,
                "remove delta on {path}"
            );

            let mut expected = value.clone();
            let expected_applied = apply_path_push(&mut expected, &path, new_value.clone());
            let (next, applied, delta) = array_push_path(&doc, &path, Arc::clone(&new_doc)).unwrap();
            assert_eq!(applied, expected_applied, "push applied on {path}");
            assert_eq!(next.to_value(), expected, "push result on {path}");
            assert_eq!(
                delta,
                next.approx_bytes() as isize - doc.approx_bytes() as isize,
                "push delta on {path}"
            );
        }
    }

    #[test]
    fn mutation_parity_against_lib_rs() {
        mutation_parity(600);
    }

    fn comparison_parity(iterations: usize) {
        fastrand::seed(0xC0FF_EE01);
        for _ in 0..iterations {
            let left = random_value(0);
            let right = random_value(0);
            assert_eq!(
                eq_docval(&build(&left), &build(&right)),
                values_equal(&left, &right),
                "eq {left:?} vs {right:?}"
            );
            assert_eq!(
                eq_json_value(&build(&left), &right),
                values_equal(&left, &right),
                "eq_json {left:?} vs {right:?}"
            );
            assert_eq!(
                cmp_docval(&build(&left), &build(&right)),
                value_cmp(&left, &right),
                "cmp {left:?} vs {right:?}"
            );
            assert_eq!(
                build(&left).ordered_key(),
                crate::ordered_value(&left),
                "ordered_key {left:?}"
            );
        }
    }

    #[test]
    fn comparison_parity_against_lib_rs() {
        comparison_parity(800);
        // Fixed pairs covering the ordering edges.
        let edge = [
            Value::Null,
            json!(false),
            json!(true),
            json!(-0.0),
            json!(0),
            json!(1.0),
            json!(""),
            json!("a"),
            json!([]),
            json!([1]),
            json!({}),
            json!({"a": 1}),
        ];
        for left in &edge {
            for right in &edge {
                assert_eq!(
                    cmp_docval(&build(left), &build(right)),
                    value_cmp(left, right)
                );
                assert_eq!(
                    eq_docval(&build(left), &build(right)),
                    values_equal(left, right)
                );
            }
        }
    }

    fn random_ast(fields: &[&str], depth: usize) -> Value {
        if depth > 0 && fastrand::u8(0..=3) == 0 {
            return match fastrand::u8(0..=2) {
                0 => json!({ "$and": [
                    json!({ (fields[fastrand::usize(0..fields.len())]): random_value(1) }),
                    random_ast(fields, depth - 1),
                ]}),
                1 => json!({ "$or": [
                    json!({ (fields[fastrand::usize(0..fields.len())]): random_value(1) }),
                    random_ast(fields, depth - 1),
                ]}),
                _ => json!({ "$not": random_ast(fields, depth - 1) }),
            };
        }
        let field = fields[fastrand::usize(0..fields.len())];
        let operand = random_value(1);
        match fastrand::u8(0..=10) {
            0 => json!({ (field): { "$eq": operand } }),
            1 => json!({ (field): { "$ne": operand } }),
            2 => json!({ (field): { "$gt": operand } }),
            3 => json!({ (field): { "$gte": operand } }),
            4 => json!({ (field): { "$lt": operand } }),
            5 => json!({ (field): { "$lte": operand } }),
            6 => json!({ (field): { "$in": [operand, random_value(1)] } }),
            7 => json!({ (field): { "$nin": [operand, random_value(1)] } }),
            8 => json!({ (field): { "$exists": fastrand::bool() } }),
            9 => json!({ (field): { "$exists": 3 } }),
            _ => json!({ (field): operand }),
        }
    }

    #[test]
    fn filter_parity_against_lib_rs() {
        fastrand::seed(0xF117_0001);
        // A literal corpus that guarantees array membership and missing-field
        // branches are exercised regardless of the PRNG.
        let fixed = [
            json!({"items": ["a", "b"], "n": 5, "meta": {"k": "v"}}),
            json!({"items": [], "n": 1.0}),
            json!({"a": [1, {"b": 2}], "n": null}),
        ];
        let asts = [
            json!({"items": {"$in": ["b"]}}),
            json!({"items": {"$nin": ["b"]}}),
            json!({"items": {"$eq": ["a", "b"]}}),
            json!({"missing": {"$exists": false}}),
            json!({"missing": {"$exists": true}}),
            json!({"meta.k": "v"}),
            json!({"items.0": "a"}),
            json!({"$and": [{"n": {"$gte": 1}}, {"items": {"$exists": true}}]}),
            json!({"$or": [{"n": {"$lt": 0}}, {"n": {"$eq": 5}}]}),
            json!({"$not": {"n": {"$gt": 0}}}),
            // A combinator short-circuits its siblings, exactly as
            // `query_matches` does (`$and` wins over a stray field key).
            json!({"$and": [{"n": {"$gte": 0}}], "n": 999}),
            json!({"missing": {"$eq": 1}}),
            json!({"missing": {"$in": [1]}}),
            json!({"missing": {"$nin": [1]}}),
            json!([{"n": {"$gte": 0}}, {"n": {"$lte": 5}}]),
            json!({"n": {"$lte": 5.0}}),
            json!({"n": {"$eq": 1}}),
            json!({"n": {"$unknown-ish": 1}}),
            json!("not an object"),
            json!(7),
        ];
        for doc in &fixed {
            let built = build(doc);
            for ast in &asts {
                assert_eq!(
                    built.matches_filter(ast),
                    query_matches(doc, ast),
                    "ast {ast} on {doc}"
                );
            }
        }

        for _ in 0..400 {
            let doc = random_value(0);
            let ast = random_ast(&KEYS, 2);
            assert_eq!(
                build(&doc).matches_filter(&ast),
                query_matches(&doc, &ast),
                "ast {ast} on {doc}"
            );
        }
    }

    #[test]
    fn projection_matches_project_doc() {
        fastrand::seed(0x9A0B_1E01);
        let mut fields: Vec<String> = KEYS.iter().map(|k| k.to_string()).collect();
        fields.extend([
            "items.0".to_string(),
            "meta.k".to_string(),
            "a.b.c".to_string(),
            "_id".to_string(),
            "n".to_string(),
        ]);
        for _ in 0..250 {
            let mut value = random_value(0);
            if let Value::Object(map) = &mut value {
                map.insert("_id".to_string(), json!("id-1"));
            }
            let doc = build(&value);
            let count = fastrand::usize(0..=fields.len());
            let picked: Vec<String> = (0..count)
                .map(|_| fields[fastrand::usize(0..fields.len())].clone())
                .collect();
            let mut out = Vec::new();
            doc.project_write_json(&picked, &mut out);
            let expected = serde_json::to_vec(&project_doc(&value, &picked)).unwrap();
            assert_eq!(
                out,
                expected,
                "projection {picked:?} on {value}:\n got {}\nwant {}",
                String::from_utf8_lossy(&out),
                String::from_utf8_lossy(&expected)
            );
        }
        // Explicit: a non-object root has no projection entries.
        let mut out = Vec::new();
        build(&json!([1, 2])).project_write_json(&["0".to_string()], &mut out);
        assert_eq!(
            out,
            serde_json::to_vec(&project_doc(&json!([1, 2]), &["0".to_string()])).unwrap()
        );
    }

    // ── Structural sharing & delta proof on a realistic document ───────────

    fn conversation(messages: usize) -> Value {
        let body = "lorem ipsum dolor sit amet consectetur adipiscing elit ".repeat(100);
        let mut list = Vec::with_capacity(messages);
        for index in 0..messages {
            list.push(json!({
                "role": if index % 2 == 0 { "user" } else { "assistant" },
                "content": format!("[{index}] {body}"),
                "ts": 1_700_000_000u64 + index as u64,
                "meta": {"tokens": index, "model": "gpt-x", "tags": ["a", "b", "c"]},
            }));
        }
        json!({"title": "conversation", "messages": list, "created": 1u64})
    }

    fn messages(node: &Arc<DocVal>) -> Arc<Vec<Arc<DocVal>>> {
        match node.as_ref() {
            DocVal::Object(map) => match map.get("messages").map(|m| m.as_ref()) {
                Some(DocVal::Array(items)) => Arc::clone(items),
                other => panic!("messages is not an array: {other:?}"),
            },
            other => panic!("root is not an object: {other:?}"),
        }
    }

    #[test]
    fn structural_sharing_and_delta_on_4mb_conversation() {
        const COUNT: usize = 767;
        let value = conversation(COUNT);
        let root = build(&value);
        let original = messages(&root);
        assert_eq!(original.len(), COUNT);

        let record = DocRecord::new(Arc::from("conv-1"), 7, Arc::clone(&root));
        assert!(
            record.approx_bytes > 3_500_000,
            "document is {} bytes, expected a ~4 MB conversation",
            record.approx_bytes
        );

        // ── array_push_path("messages", msg) ──────────────────────────────
        let message = json!({
            "role": "user",
            "content": "[767] a brand new message",
            "ts": 1_700_000_767u64,
            "meta": {"tokens": 767, "model": "gpt-x", "tags": ["a"]},
        });
        let message_doc = build(&message);
        let (pushed_root, applied, push_delta) =
            array_push_path(&root, "messages", Arc::clone(&message_doc)).unwrap();
        assert!(applied);

        let pushed = messages(&pushed_root);
        assert_eq!(pushed.len(), COUNT + 1);
        // Every untouched message is the *same* allocation — the push copied
        // the top-level object and the messages array, nothing else.
        for index in 0..COUNT {
            assert!(
                Arc::ptr_eq(&original[index], &pushed[index]),
                "message {index} was copied by array_push_path"
            );
        }
        assert!(Arc::ptr_eq(&message_doc, &pushed[COUNT]));
        // The root object was copied, but its other values were not.
        match (root.as_ref(), pushed_root.as_ref()) {
            (DocVal::Object(before), DocVal::Object(after)) => {
                for key in ["title", "created"] {
                    assert!(
                        Arc::ptr_eq(before.get(key).expect(key), after.get(key).expect(key)),
                        "root field {key} was copied"
                    );
                }
            }
            _ => panic!("expected object roots"),
        }
        // The delta is exactly the new message plus one slot — no full walk.
        assert_eq!(
            push_delta,
            (message_doc.approx_bytes() + SLOT_COST) as isize
        );
        assert!(push_delta > 0);
        // Structural proof that nothing else was charged: the delta is tiny
        // relative to the document and equals the recomputed difference.
        assert_eq!(
            push_delta,
            pushed_root.approx_bytes() as isize - root.approx_bytes() as isize
        );
        assert!((push_delta as usize) * 100 < record.approx_bytes);

        let pushed_record = DocRecord::with_delta(&record, 8, Arc::clone(&pushed_root), push_delta);
        assert_eq!(pushed_record.id, record.id);
        assert_eq!(pushed_record.doc_seq, 8);
        assert_eq!(
            pushed_record.approx_bytes,
            DocRecord::new(Arc::from("conv-1"), 8, Arc::clone(&pushed_root)).approx_bytes,
            "delta-maintained approx_bytes diverged from a fresh measurement"
        );
        assert_eq!(
            pushed_record.approx_bytes,
            record.approx_bytes + push_delta as usize
        );

        // ── set_path("messages.500.content", ...) ─────────────────────────
        let replacement = json!("[500] the edited message content");
        let replacement_doc = build(&replacement);
        let (edited_root, applied, set_delta) =
            set_path(&pushed_root, "messages.500.content", Arc::clone(&replacement_doc)).unwrap();
        assert!(applied);

        let edited = messages(&edited_root);
        assert_eq!(edited.len(), COUNT + 1);
        for index in 0..=COUNT {
            if index == 500 {
                continue;
            }
            assert!(
                Arc::ptr_eq(&pushed[index], &edited[index]),
                "message {index} was copied by set_path"
            );
        }
        assert!(!Arc::ptr_eq(&pushed[500], &edited[500]));
        assert_eq!(
            edited[500].get_path("content").map(|v| v.to_value()),
            Some(replacement)
        );
        // Sibling fields of the edited message are shared with the old version.
        let old_message = match pushed[500].as_ref() {
            DocVal::Object(map) => Arc::clone(map),
            other => panic!("message is not an object: {other:?}"),
        };
        let new_message = match edited[500].as_ref() {
            DocVal::Object(map) => Arc::clone(map),
            other => panic!("message is not an object: {other:?}"),
        };
        for key in ["role", "ts", "meta"] {
            assert!(
                Arc::ptr_eq(
                    old_message.get(key).expect("sibling field"),
                    new_message.get(key).expect("sibling field")
                ),
                "sibling field {key} was copied"
            );
        }
        // Exact delta: replaced content subtree only.
        let old_content = old_message.get("content").expect("content").approx_bytes();
        assert_eq!(
            set_delta,
            replacement_doc.approx_bytes() as isize - old_content as isize
        );
        assert_eq!(
            set_delta,
            edited_root.approx_bytes() as isize - pushed_root.approx_bytes() as isize
        );

        let edited_record =
            DocRecord::with_delta(&pushed_record, 9, Arc::clone(&edited_root), set_delta);
        assert_eq!(
            edited_record.approx_bytes,
            DocRecord::new(Arc::from("conv-1"), 9, Arc::clone(&edited_root)).approx_bytes
        );

        // The pre-push tree is still intact — publication never mutates.
        assert_eq!(messages(&root).len(), COUNT);
        assert_eq!(
            root.get_path("messages.500.content").map(|v| v.to_value()),
            Some(value["messages"][500]["content"].clone())
        );
    }

    #[test]
    fn docrecord_with_delta_rejects_underflow() {
        let record = DocRecord::new(Arc::from("x"), 1, build(&json!({"a": 1})));
        let smaller = build(&json!({}));
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            DocRecord::with_delta(&record, 2, Arc::clone(&smaller), isize::MIN)
        }));
        assert!(result.is_err(), "underflowing approx_bytes must abort loudly");
    }
}
