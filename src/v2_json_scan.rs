//! Zero-allocation raw-JSON field reader for the v2 engine.
//!
//! A v2 document is compact UTF-8 JSON bytes, validated when it was written.
//! Query planning, filtering, sorting and projection therefore never need a
//! `serde_json::Value` for a document they do not return: this module walks the
//! bytes directly, comparing scalars in place and copying out only the field a
//! caller actually asked for.
//!
//! Equivalence with the in-memory evaluator (`query_matches`) is exact:
//! - every operator mirrors `evaluate_condition`/`values_equal`/`value_cmp`;
//! - anything this scanner cannot interpret (a non-object root, a malformed
//!   escape, a structural surprise) returns `None`, and the callers fall back
//!   to `serde_json::from_slice` + `query_matches`.

use crate::v2_format::OrderedValue;
use serde_json::{Map, Value};
use std::cmp::Ordering;

fn is_ws(byte: u8) -> bool {
    matches!(byte, b' ' | b'\t' | b'\n' | b'\r')
}

fn trim_ws(bytes: &[u8]) -> &[u8] {
    let start = bytes.iter().position(|b| !is_ws(*b)).unwrap_or(bytes.len());
    let end = bytes
        .iter()
        .rposition(|b| !is_ws(*b))
        .map_or(start, |position| position + 1);
    &bytes[start..end]
}

// ─── Byte cursor ────────────────────────────────────────────────────

const LO: u64 = 0x0101_0101_0101_0101;
const HI: u64 = 0x8080_8080_8080_8080;

/// Word-at-a-time search for the first `"` or `\` at or after `position`.
///
/// Document bytes are dominated by string values, so skipping those strings
/// eight bytes at a time is what keeps a whole-document scan cheap.
fn find_quote_or_escape(bytes: &[u8], mut position: usize) -> Option<usize> {
    while position + 8 <= bytes.len() {
        let word = u64::from_le_bytes(
            bytes[position..position + 8]
                .try_into()
                .expect("8-byte window"),
        );
        let quotes = word ^ 0x2222_2222_2222_2222;
        let slashes = word ^ 0x5c5c_5c5c_5c5c_5c5c;
        let hits =
            (quotes.wrapping_sub(LO) & !quotes & HI) | (slashes.wrapping_sub(LO) & !slashes & HI);
        if hits != 0 {
            return Some(position + (hits.trailing_zeros() as usize) / 8);
        }
        position += 8;
    }
    while position < bytes.len() {
        if matches!(bytes[position], b'"' | b'\\') {
            return Some(position);
        }
        position += 1;
    }
    None
}

struct Cursor<'a> {
    bytes: &'a [u8],
    pos: usize,
}

impl<'a> Cursor<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Cursor { bytes, pos: 0 }
    }

    fn peek(&self) -> Option<u8> {
        self.bytes.get(self.pos).copied()
    }

    fn skip_ws(&mut self) {
        while self.peek().map_or(false, is_ws) {
            self.pos += 1;
        }
    }

    fn literal(&mut self, text: &[u8]) -> Option<()> {
        if self.bytes.get(self.pos..self.pos + text.len())? == text {
            self.pos += text.len();
            Some(())
        } else {
            None
        }
    }

    /// Advance past a JSON string; `Some(inner)` is the raw (escaped) content.
    fn take_string(&mut self) -> Option<&'a [u8]> {
        if self.peek()? != b'"' {
            return None;
        }
        let start = self.pos + 1;
        let mut position = start;
        loop {
            let hit = find_quote_or_escape(self.bytes, position)?;
            if self.bytes[hit] == b'"' {
                self.pos = hit + 1;
                return Some(&self.bytes[start..hit]);
            }
            position = hit + 2;
            if position > self.bytes.len() {
                return None;
            }
        }
    }

    fn skip_number(&mut self) -> Option<()> {
        let start = self.pos;
        while let Some(byte) = self.peek() {
            if byte.is_ascii_digit() || matches!(byte, b'-' | b'+' | b'.' | b'e' | b'E') {
                self.pos += 1;
            } else {
                break;
            }
        }
        if self.pos == start {
            None
        } else {
            Some(())
        }
    }

    fn skip_container(&mut self) -> Option<()> {
        let mut depth = 0usize;
        loop {
            match self.peek()? {
                b'"' => {
                    self.take_string()?;
                }
                b'{' | b'[' => {
                    depth += 1;
                    self.pos += 1;
                }
                b'}' | b']' => {
                    depth = depth.checked_sub(1)?;
                    self.pos += 1;
                    if depth == 0 {
                        return Some(());
                    }
                }
                _ => self.pos += 1,
            }
        }
    }

    fn skip_value(&mut self) -> Option<()> {
        self.skip_ws();
        match self.peek()? {
            b'"' => self.take_string().map(|_| ()),
            b'{' | b'[' => self.skip_container(),
            b't' => self.literal(b"true"),
            b'f' => self.literal(b"false"),
            b'n' => self.literal(b"null"),
            _ => self.skip_number(),
        }
    }
}

// ─── Field lookup ───────────────────────────────────────────────────

/// Result of resolving a dot path in one document.
enum Found<'a> {
    /// The path resolved; the slice is the value's raw JSON text.
    Value(&'a [u8]),
    /// The document is well-formed but the path does not exist.
    Missing,
}

fn decode_string(inner: &[u8]) -> Option<String> {
    // Exact escape handling for the rare escaped case: let serde_json decode.
    let mut quoted = Vec::with_capacity(inner.len() + 2);
    quoted.push(b'"');
    quoted.extend_from_slice(inner);
    quoted.push(b'"');
    serde_json::from_slice::<String>(&quoted).ok()
}

fn object_field<'a>(object: &'a [u8], key: &str) -> Option<Option<&'a [u8]>> {
    let mut cursor = Cursor::new(object);
    cursor.skip_ws();
    if cursor.peek()? != b'{' {
        return None;
    }
    cursor.pos += 1;
    loop {
        cursor.skip_ws();
        match cursor.peek()? {
            b'}' => return Some(None),
            b',' => {
                cursor.pos += 1;
                continue;
            }
            b'"' => {}
            _ => return None,
        }
        let name = cursor.take_string()?;
        let matches = if name == key.as_bytes() {
            true
        } else if name.contains(&b'\\') {
            decode_string(name)?.as_str() == key
        } else {
            false
        };
        cursor.skip_ws();
        if cursor.peek()? != b':' {
            return None;
        }
        cursor.pos += 1;
        cursor.skip_ws();
        let start = cursor.pos;
        cursor.skip_value()?;
        let end = cursor.pos;
        if matches {
            return Some(Some(&object[start..end]));
        }
    }
}

fn array_element<'a>(array: &'a [u8], index: usize) -> Option<Option<&'a [u8]>> {
    let mut cursor = Cursor::new(array);
    cursor.skip_ws();
    if cursor.peek()? != b'[' {
        return None;
    }
    cursor.pos += 1;
    let mut position = 0usize;
    loop {
        cursor.skip_ws();
        match cursor.peek()? {
            b']' => return Some(None),
            b',' => {
                cursor.pos += 1;
                continue;
            }
            _ => {}
        }
        let start = cursor.pos;
        cursor.skip_value()?;
        let end = cursor.pos;
        if position == index {
            return Some(Some(&array[start..end]));
        }
        position += 1;
    }
}

/// Resolve a dot path. `None` = the document could not be interpreted;
/// `Some(Missing)` = a valid document without that path.
fn lookup<'a>(doc: &'a [u8], path: &str) -> Option<Found<'a>> {
    let mut current: &'a [u8] = doc;
    for segment in path.split('.') {
        let trimmed = trim_ws(current);
        let found = match trimmed.first()? {
            b'{' => object_field(trimmed, segment)?,
            b'[' => match segment.parse::<usize>() {
                Ok(index) => array_element(trimmed, index)?,
                Err(_) => return Some(Found::Missing),
            },
            _ => return Some(Found::Missing),
        };
        match found {
            Some(value) => current = value,
            None => return Some(Found::Missing),
        }
    }
    Some(Found::Value(current))
}

// ─── Raw value view ─────────────────────────────────────────────────

#[derive(Clone, Copy)]
struct Raw<'a> {
    bytes: &'a [u8],
}

impl<'a> Raw<'a> {
    fn new(bytes: &'a [u8]) -> Self {
        Raw {
            bytes: trim_ws(bytes),
        }
    }

    fn first(&self) -> Option<u8> {
        self.bytes.first().copied()
    }

    fn is_null(&self) -> bool {
        self.bytes == b"null"
    }

    fn as_bool(&self) -> Option<bool> {
        match self.bytes {
            b"true" => Some(true),
            b"false" => Some(false),
            _ => None,
        }
    }

    fn as_number(&self) -> Option<f64> {
        std::str::from_utf8(self.bytes).ok()?.parse::<f64>().ok()
    }

    fn is_array(&self) -> bool {
        self.first() == Some(b'[')
    }

    /// The decoded string, or `None` when this is not a string value.
    fn as_string(&self) -> Option<String> {
        let inner = self
            .bytes
            .strip_prefix(b"\"")?
            .strip_suffix(b"\"")?;
        if inner.contains(&b'\\') {
            decode_string(inner)
        } else {
            std::str::from_utf8(inner).ok().map(str::to_string)
        }
    }

    fn elements(&self) -> Elements<'a> {
        let mut cursor = Cursor::new(self.bytes);
        cursor.pos = 1; // past '['
        Elements {
            source: self.bytes,
            cursor,
            done: false,
        }
    }

    fn to_value(&self) -> Option<Value> {
        serde_json::from_slice(self.bytes).ok()
    }
}

struct Elements<'a> {
    source: &'a [u8],
    cursor: Cursor<'a>,
    done: bool,
}

impl<'a> Iterator for Elements<'a> {
    type Item = Raw<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        self.cursor.skip_ws();
        match self.cursor.peek()? {
            b']' => return None,
            b',' => {
                self.cursor.pos += 1;
                self.cursor.skip_ws();
            }
            _ => {}
        }
        let start = self.cursor.pos;
        self.cursor.skip_value()?;
        let end = self.cursor.pos;
        Some(Raw::new(&self.source[start..end]))
    }
}

// ─── Comparison ─────────────────────────────────────────────────────

fn rank_of(value: &Value) -> u8 {
    match value {
        Value::Null => 0,
        Value::Bool(_) => 1,
        Value::Number(_) => 2,
        Value::String(_) => 3,
        _ => 4,
    }
}

fn raw_rank(raw: Raw<'_>) -> u8 {
    match raw.first() {
        Some(b'n') => 0,
        Some(b't') | Some(b'f') => 1,
        Some(b'"') => 3,
        Some(b'[') | Some(b'{') => 4,
        _ => 2,
    }
}

fn raw_equals(raw: Raw<'_>, operand: &Value) -> bool {
    match operand {
        Value::Null => raw.is_null(),
        Value::Bool(flag) => raw.as_bool() == Some(*flag),
        Value::Number(number) => match (raw.as_number(), number.as_f64()) {
            (Some(left), Some(right)) => left == right,
            _ => false,
        },
        Value::String(text) => raw.as_string().map_or(false, |value| value == *text),
        other => raw
            .to_value()
            .map_or(false, |value| crate::values_equal(&value, other)),
    }
}

fn raw_cmp(raw: Raw<'_>, operand: &Value) -> Ordering {
    let raw_rank = raw_rank(raw);
    let operand_rank = rank_of(operand);
    if raw_rank != operand_rank {
        return raw_rank.cmp(&operand_rank);
    }
    match operand {
        Value::Null => Ordering::Equal,
        Value::Bool(flag) => raw.as_bool().unwrap_or(*flag).cmp(flag),
        Value::Number(number) => {
            let left = raw.as_number().unwrap_or(f64::NAN);
            let right = number.as_f64().unwrap_or(f64::NAN);
            left.total_cmp(&right)
        }
        Value::String(text) => raw.as_string().as_deref().unwrap_or("").cmp(text.as_str()),
        other => {
            let left = raw.to_value().unwrap_or(Value::Null).to_string();
            let right = other.to_string();
            left.cmp(&right)
        }
    }
}

fn raw_in(raw: Raw<'_>, operand: &Value) -> bool {
    let Some(values) = operand.as_array() else {
        return false;
    };
    if raw.is_array() {
        raw.elements()
            .any(|element| values.iter().any(|value| raw_equals(element, value)))
    } else {
        values.iter().any(|value| raw_equals(raw, value))
    }
}

fn eval_condition(raw: Raw<'_>, condition: &Value) -> bool {
    let Value::Object(ops) = condition else {
        return raw_equals(raw, condition);
    };
    ops.iter().all(|(op, operand)| match op.as_str() {
        "$eq" => raw_equals(raw, operand),
        "$ne" => !raw_equals(raw, operand),
        "$gt" => raw_cmp(raw, operand) == Ordering::Greater,
        "$gte" => raw_cmp(raw, operand) != Ordering::Less,
        "$lt" => raw_cmp(raw, operand) == Ordering::Less,
        "$lte" => raw_cmp(raw, operand) != Ordering::Greater,
        "$in" => raw_in(raw, operand),
        "$nin" => !raw_in(raw, operand),
        "$exists" => operand.as_bool().unwrap_or(true),
        // Unreachable for a validated AST; mirrors the in-memory catch-all.
        _ => true,
    })
}

fn eval(bytes: &[u8], ast: &Value) -> Option<bool> {
    match ast {
        Value::Object(map) => {
            if let Some(and_expr) = map.get("$and") {
                for condition in and_expr.as_array()? {
                    if !eval(bytes, condition)? {
                        return Some(false);
                    }
                }
                return Some(true);
            }
            if let Some(or_expr) = map.get("$or") {
                for condition in or_expr.as_array()? {
                    if eval(bytes, condition)? {
                        return Some(true);
                    }
                }
                return Some(false);
            }
            if let Some(not_expr) = map.get("$not") {
                return Some(!eval(bytes, not_expr)?);
            }
            for (field, condition) in map {
                match lookup(bytes, field)? {
                    Found::Value(raw) => {
                        if !eval_condition(Raw::new(raw), condition) {
                            return Some(false);
                        }
                    }
                    Found::Missing => {
                        if let Value::Object(ops) = condition {
                            if let Some(exists) = ops.get("$exists") {
                                if !exists.as_bool().unwrap_or(true) {
                                    continue;
                                }
                            }
                        }
                        return Some(false);
                    }
                }
            }
            Some(true)
        }
        Value::Array(conditions) => {
            for condition in conditions {
                if !eval(bytes, condition)? {
                    return Some(false);
                }
            }
            Some(true)
        }
        _ => Some(false),
    }
}

// ─── Public surface ─────────────────────────────────────────────────

/// `query_matches` over raw document bytes.
pub(crate) fn matches(bytes: &[u8], ast: &Value) -> bool {
    match eval(bytes, ast) {
        Some(result) => result,
        None => serde_json::from_slice::<Value>(bytes)
            .map(|doc| crate::query_matches(&doc, ast))
            .unwrap_or(false),
    }
}

/// The value at `field`, parsed in isolation (the rest of the document is
/// never materialized).
pub(crate) fn field_value(bytes: &[u8], field: &str) -> Option<Value> {
    match lookup(bytes, field)? {
        Found::Value(raw) => Raw::new(raw).to_value(),
        Found::Missing => None,
    }
}

/// The sort key at `field`, exactly as `field_get` + `ordered_value` would
/// produce it (a missing field sorts as JSON null).
pub(crate) fn sort_key(bytes: &[u8], field: &str) -> Option<OrderedValue> {
    let raw = match lookup(bytes, field) {
        Some(Found::Value(raw)) => Raw::new(raw),
        Some(Found::Missing) => return Some(OrderedValue::Null),
        None => {
            return serde_json::from_slice::<Value>(bytes).ok().map(|doc| {
                crate::field_get(&doc, field)
                    .map(OrderedValue::from_json)
                    .unwrap_or(OrderedValue::Null)
            })
        }
    };
    Some(match raw.first()? {
        b'n' => OrderedValue::Null,
        b't' | b'f' => OrderedValue::Bool(raw.as_bool()?),
        b'"' => OrderedValue::String(raw.as_string()?),
        b'[' | b'{' => OrderedValue::Composite(raw.to_value()?.to_string()),
        _ => OrderedValue::Number(raw.as_number()?),
    })
}

/// The requested fields (dot paths kept verbatim, plus `_id`) as one object —
/// the same shape `project_doc` produces, without parsing the rest.
pub(crate) fn project(bytes: &[u8], fields: &[String]) -> Value {
    let mut out = Map::new();
    for field in fields {
        if let Some(value) = field_value(bytes, field) {
            out.insert(field.clone(), value);
        }
    }
    if let Some(id) = field_value(bytes, "_id") {
        out.insert("_id".to_string(), id);
    }
    Value::Object(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const DOC: &[u8] = br#"{"_id":"a","n":1,"f":2.5,"s":"hi","b":true,"nil":null,"tags":["x","y"],"nested":{"deep":{"k":7}},"list":[{"v":1},{"v":2}],"esc":"line\nbreak","uni":"caf\u00e9"}"#;

    #[test]
    fn resolves_top_level_and_dotted_paths() {
        assert_eq!(field_value(DOC, "n"), Some(json!(1)));
        assert_eq!(field_value(DOC, "nested.deep.k"), Some(json!(7)));
        assert_eq!(field_value(DOC, "list.1.v"), Some(json!(2)));
        assert_eq!(field_value(DOC, "missing"), None);
        assert_eq!(field_value(DOC, "nested.missing"), None);
        assert_eq!(field_value(DOC, "nested.deep.k.deeper"), None);
        assert_eq!(field_value(DOC, "list.9"), None);
    }

    #[test]
    fn decodes_escaped_strings() {
        assert_eq!(field_value(DOC, "esc"), Some(json!("line\nbreak")));
        assert_eq!(field_value(DOC, "uni"), Some(json!("café")));
    }

    #[test]
    fn matches_scalar_predicates() {
        assert!(matches(DOC, &json!({"n": 1})));
        assert!(matches(DOC, &json!({"n": {"$eq": 1}})));
        assert!(matches(DOC, &json!({"f": {"$gt": 2, "$lt": 3}})));
        assert!(matches(DOC, &json!({"n": {"$ne": 2}})));
        assert!(matches(DOC, &json!({"s": {"$in": ["hi", "bye"]}})));
        assert!(!matches(DOC, &json!({"s": {"$nin": ["hi"]}})));
        assert!(matches(DOC, &json!({"b": true})));
        assert!(matches(DOC, &json!({"nil": null})));
        assert!(matches(DOC, &json!({"missing": {"$exists": false}})));
        assert!(!matches(DOC, &json!({"missing": {"$exists": true}})));
        assert!(matches(DOC, &json!({"nested.deep.k": 7})));
    }

    #[test]
    fn matches_arrays_like_the_in_memory_evaluator() {
        // `$in` against an array field matches an element.
        assert!(matches(DOC, &json!({"tags": {"$in": ["y"]}})));
        // `$eq` on an array field compares the whole array.
        assert!(matches(DOC, &json!({"tags": ["x", "y"]})));
        assert!(!matches(DOC, &json!({"tags": "x"})));
        // Array-aware ordering by canonical JSON text.
        assert!(matches(DOC, &json!({"tags": {"$gt": ["a"]}})));
    }

    #[test]
    fn matches_combinators() {
        assert!(matches(DOC, &json!({"$and": [{"n": 1}, {"b": true}]})));
        assert!(matches(DOC, &json!({"$or": [{"n": 9}, {"s": "hi"}]})));
        assert!(matches(DOC, &json!({"$not": {"n": 2}})));
        assert!(matches(DOC, &json!({"$and": [{"n": 1}]})));
    }

    #[test]
    fn sort_keys_match_ordered_value_semantics() {
        assert_eq!(sort_key(DOC, "n"), Some(OrderedValue::Number(1.0)));
        assert_eq!(
            sort_key(DOC, "s"),
            Some(OrderedValue::String("hi".to_string()))
        );
        assert_eq!(sort_key(DOC, "b"), Some(OrderedValue::Bool(true)));
        assert_eq!(sort_key(DOC, "nil"), Some(OrderedValue::Null));
        assert_eq!(sort_key(DOC, "missing"), Some(OrderedValue::Null));
        assert_eq!(
            sort_key(DOC, "tags"),
            Some(OrderedValue::Composite("[\"x\",\"y\"]".to_string()))
        );
    }

    #[test]
    fn projects_only_the_requested_fields() {
        let projected = project(DOC, &["n".to_string(), "nested.deep.k".to_string()]);
        assert_eq!(projected, json!({"n": 1, "nested.deep.k": 7, "_id": "a"}));
    }

    #[test]
    fn falls_back_cleanly_on_a_non_object_root() {
        let bytes = br#"[1,2,3]"#;
        assert!(!matches(bytes, &json!({"a": 1})));
        // The in-memory evaluator accepts an array root as an implicit $and.
        assert!(matches(bytes, &json!([])));
        assert!(matches(bytes, &json!([{"0": 1}])));
    }
}
