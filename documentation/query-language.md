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
    }
);
```

This returns active users, sorted by age descending, skipping the first 10, limited to 20 results.

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
| `query(ast)` | N/A | Always O(n) full scan |

The JSON AST query evaluator always performs a full scan over all documents. For hot queries, create an index and use `find()` instead.

```rust
// Slow: full scan every time
db.query(json!({"email": {"$eq": "alice@example.com"}}))

// Fast: O(1) with hash index
db.create_index("email")?;
db.find("email", &json!("alice@example.com"))
```
