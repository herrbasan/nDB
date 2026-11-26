# nDB Query Language Reference

> Complete guide to nDB's JSON AST query system (Layer 3).

---

## Overview

nDB queries are **plain JSON objects** that express filter conditions. There is no builder pattern, no special syntax — just JSON that passes directly from your API to the evaluator.

```rust
let results = db.query(json!({
    "status": {"$eq": "active"},
    "age": {"$gte": 18}
}));
```

---

## Implicit Equality

The simplest query matches a field to a value directly:

```json
{"field": "value"}
```

This is equivalent to `{"field": {"$eq": "value"}}`.

**Example:**
```rust
// Find all documents where status equals "active"
db.query(json!({"status": "active"}))
```

---

## Comparison Operators

All operators live inside a JSON object as the key, with the comparison value as the value.

| Operator | Meaning | Works With |
|----------|---------|------------|
| `$eq` | Equal | Any type |
| `$ne` | Not equal | Any type |
| `$gt` | Greater than | Numbers, strings |
| `$gte` | Greater than or equal | Numbers, strings |
| `$lt` | Less than | Numbers, strings |
| `$lte` | Less than or equal | Numbers, strings |
| `$in` | Value in array | Any type |
| `$nin` | Value not in array | Any type |
| `$exists` | Field exists (true/false) | Boolean |

### Examples

```rust
// Age >= 21
db.query(json!({"age": {"$gte": 21}}))

// Score between 50 and 100
db.query(json!({"score": {"$gte": 50, "$lte": 100}}))

// Status is one of ["active", "pending"]
db.query(json!({"status": {"$in": ["active", "pending"]}}))

// Category is NOT one of ["spam", "deleted"]
db.query(json!({"category": {"$nin": ["spam", "deleted"]}}))

// Field "email" exists
db.query(json!({"email": {"$exists": true}}))

// Field "deleted_at" does NOT exist
db.query(json!({"deleted_at": {"$exists": false}}))
```

### Multiple Operators on One Field

You can combine operators on a single field:

```rust
// Score >= 50 AND score <= 100
db.query(json!({"score": {"$gte": 50, "$lte": 100}}))
```

All operators must match (AND logic).

---

## Logical Combinators

### `$and` — All conditions must match

```json
{"$and": [condition1, condition2, ...]}
```

```rust
db.query(json!({
    "$and": [
        {"status": {"$eq": "active"}},
        {"age": {"$gte": 21}}
    ]
}))
```

### `$or` — At least one condition must match

```json
{"$or": [condition1, condition2, ...]}
```

```rust
db.query(json!({
    "$or": [
        {"role": "admin"},
        {"role": "moderator"}
    ]
}))
```

### `$not` — Negate a condition

```json
{"$not": condition}
```

```rust
db.query(json!({
    "$not": {"status": "banned"}
}))
```

---

## Implicit `$and`

A query object with multiple fields uses AND logic automatically:

```rust
// status = "active" AND age >= 18
db.query(json!({
    "status": "active",
    "age": {"$gte": 18}
}))
```

This is equivalent to:

```rust
db.query(json!({
    "$and": [
        {"status": "active"},
        {"age": {"$gte": 18}}
    ]
}))
```

---

## Array at Top Level = Implicit `$and`

If you pass an array at the top level, all conditions must match:

```rust
db.query(json!([
    {"status": "active"},
    {"age": {"$gte": 18}}
]))
```

---

## Dot Notation (Nested Fields)

Access nested fields using dot notation:

```rust
db.query(json!({
    "user.name": {"$eq": "Alice"},
    "address.city": "Berlin"
}))
```

Given a document:
```json
{
    "user": {"name": "Alice", "email": "alice@example.com"},
    "address": {"city": "Berlin", "country": "DE"}
}
```

---

## `query_with()` — Options

Sort, offset, and limit the results:

```rust
use ndb::{QueryOptions, SortDir};

let results = db.query_with(
    json!({"status": "active"}),
    QueryOptions {
        sort_by: Some(("age".to_string(), SortDir::Desc)),
        offset: Some(10),
        limit: Some(20),
        ..Default::default()
    },
);
```

This returns active users, sorted by age descending, skipping the first 10, limited to 20 results.

---

## Standalone query contract (Phase 4A)

`QueryOptions` carries the contract options in addition to `sort_by`/`offset`/`limit`:

| Option | Meaning |
|---|---|
| `sort_keys: Vec<SortKey>` | Deterministic multi-key order (`{field, dir, missing}`). Supersedes `sort_by`; a final `_id` ascending tie-breaker is always appended. |
| `cursor: Option<String>` | Opaque `next_cursor` from the previous page. Cannot be combined with a non-zero `offset`; a cursor whose key count disagrees with the order is refused. With no sort the order is `_id` ascending. |
| `text: Option<TextQuery>` | Basic lexical search — whitespace terms are ANDed, `"quoted text"` is a phrase, case-insensitive, over `TextQuery::fields` → the mapping's `text` fields → every string field. |
| `max_scan_docs: Option<usize>` | Scan budget: exceeding it fails with `Error::ScanBudgetExceeded` (HTTP `422 scan_budget_exceeded`) instead of appearing to hang. |

These options require a **v3 (RAM-first)** database; a v1 database refuses them loudly
(`sort_keys, cursor, text and max_scan_docs require a v3 (RAM-first) database`), never
silently ignoring them.

```rust
use ndb::{QueryOptions, SortKey, SortDir, TextQuery, MissingPlacement};

let page = db.query_raw_page(
    json!({"status": "active"}),
    QueryOptions {
        sort_keys: vec![
            SortKey { field: "score".into(), dir: SortDir::Desc, missing: MissingPlacement::Last },
            SortKey::asc("name"),
        ],
        text: Some(TextQuery { query: "rust \"document database\"".into(), fields: None }),
        max_scan_docs: Some(50_000),
        limit: Some(25),
        ..Default::default()
    },
    None,
)?;
// page.0 = QueryPageMeta { total, scanned_docs, used_indexes, next_cursor }
// page.1 = the encoded rows
```

`query_with`/`query_projected` return rows only; `query_raw_page` is the entry
point that also reports the execution accounting (`QueryPageMeta`).
`POST /v2/dbs/{name}/search` is the same contract with `text` required.
`POST /v2/dbs/{name}/query` executes an AST (filter object) — described below.

---

## Type Comparison Rules

### Numbers

Numbers are compared as `f64` internally. This means `1` (integer) and `1.0` (float) are considered equal.

```rust
db.query(json!({"count": 1}))     // Matches count: 1.0
db.query(json!({"count": {"$gt": 0}}))  // Matches count: 1, 2, 3, ...
```

### Strings

Strings are compared lexicographically:

```rust
db.query(json!({"name": {"$gt": "M"}}))  // Names starting M-Z
```

### Booleans

`true > false`.

### Null

`null` only matches `null` via `$eq`.

### Arrays and Objects

Compared by JSON stringification. Not recommended for querying — use `$exists` to check for presence.

---

## Complete Examples

### Find active adult users sorted by name

```rust
let users = db.query_with(
    json!({
        "status": "active",
        "age": {"$gte": 18}
    }),
    QueryOptions {
        sort_by: Some(("name".to_string(), SortDir::Asc)),
        limit: None,
        offset: None,
    }
);
```

### Complex nested query

```rust
let results = db.query(json!({
    "$and": [
        {"$or": [
            {"role": "admin"},
            {"role": "editor"}
        ]},
        {"$not": {"banned": true}},
        {"login_count": {"$gte": 5}},
        {"email": {"$exists": true}}
    ]
}));
```

### Pagination

```rust
// Page 3, 25 items per page
let page = db.query_with(
    json!({"status": "active"}),
    QueryOptions {
        sort_by: Some(("created_at".to_string(), SortDir::Desc)),
        offset: Some(50),   // (page - 1) * per_page
        limit: Some(25),
    }
);
```

---

## Performance Notes

| Query Type | Index Available | Complexity |
|------------|----------------|------------|
| `find(field, value)` | Hash index | O(1) |
| `find(field, value)` | No index | O(n) linear scan |
| `find_range(field, min, max)` | BTree index | O(log n + k) |
| `find_range(field, min, max)` | No index | O(n) linear scan |
| `query(ast)` | Planner narrows via Hash/BTree `$eq`/`$in`/range conjuncts | O(k) candidates re-checked, else O(n) full scan |

The JSON AST evaluator always re-checks every candidate with the full AST; the planner only narrows the candidate set. Unindexed queries are a full scan. For hot equality lookups, create an index and use `find()`.

```rust
// Slow: full scan every time
db.query(json!({"email": {"$eq": "alice@example.com"}}))

// Fast: O(1) with hash index
db.create_index("email")?;
db.find("email", &json!("alice@example.com"))
```

For lexical search use `QueryOptions::text` (or `POST …/search`): it is an honest scan, so bound it with `max_scan_docs` and read `scanned_docs`. Advanced ranking (BM25, vectors, hybrid, rerank) is nVDB's job.
