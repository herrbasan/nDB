# nDB Rust API Reference

> Complete API documentation for the `ndb` Rust crate.

---

## Installation

Add to your `Cargo.toml`:

```toml
[dependencies]
ndb = { path = "../ndb" }
serde_json = "1"
```

---

## Quick Start

```rust
use ndb::{Database, Persistence};
use serde_json::json;

// Open a database
let db = Database::open("mydata.jsonl")?;

// Insert a document
let id = db.insert(json!({
    "name": "Alice",
    "age": 30,
    "email": "alice@example.com"
}))?;

// Get by ID (O(1))
let doc = db.get(&id)?;
assert_eq!(doc["name"], "Alice");

// Update
db.update(&id, json!({"name": "Alice Smith", "age": 31, "email": "alice@example.com"}))?;

// Delete (soft delete / tombstone)
db.delete(&id)?;

// Restore from trash
db.restore(&id)?;
```

---

## `Database`

The main database struct. In-memory document store backed by JSON Lines persistence.

### Opening

#### `Database::open(path) -> Result<Database>`

Open or create a database at the given file path.

- If the file exists, loads all documents into memory (last write wins).
- If not, creates a new file with `_meta` header.
- Persistence defaults to `Lazy`.

```rust
let db = Database::open("data/app.jsonl")?;
```

#### `Database::open_in_memory() -> Result<Database>`

Open a purely in-memory database. No file is created. Data is lost when the `Database` is dropped.

```rust
let db = Database::open_in_memory()?;
```

### Configuration

#### `with_persistence(mode: Persistence) -> Database`

Set persistence mode. Returns `self` for chaining.

```rust
let db = Database::open("data.jsonl")?
    .with_persistence(Persistence::Immediate);
```

#### `with_trash_mode(mode: TrashMode) -> Database`

Set trash behavior. Returns `self` for chaining.

```rust
let db = Database::open("data.jsonl")?
    .with_trash_mode(TrashMode::TTL(Duration::from_secs(86400)));
```

---

## Layer 1: Core Operations

### `insert(doc: Value) -> Result<String>`

Insert a document. Generates a 16-char NanoID `_id` and returns it.

- **O(1)**: HashMap insert + file append
- Automatically updates all indexes
- Panics if `doc` is not a JSON object

```rust
let id = db.insert(json!({"title": "Hello", "count": 42}))?;
// id = "V1StGXR8Z5jdHi6B"
```

### `insert_with_prefix(prefix: &str, doc: Value) -> Result<String>`

Insert with a prefixed ID. Format: `{prefix}_{random16}`.

```rust
let id = db.insert_with_prefix("user", json!({"name": "Bob"}))?;
// id = "user_k8Tm2pQw4xNvRj7L"
```

### `get(id: &str) -> Result<Value>`

Get a document by ID. O(1) HashMap lookup.

- Returns `Error::NotFound` if the document doesn't exist or is deleted.

```rust
let doc = db.get(&id)?;
println!("{}", doc["title"]);
```

### `update(id: &str, new_doc: Value) -> Result<()>`

Replace a document. The `_id` field is preserved.

- Appends new version to file (old version superseded by index)
- Automatically updates all indexes
- Returns `Error::NotFound` if ID doesn't exist

```rust
db.update(&id, json!({"title": "Updated", "count": 43}))?;
```

### `delete(id: &str) -> Result<()>`

Soft delete (tombstone). The document is removed from the in-memory store and a tombstone entry is appended to the file.

- O(1) operation
- Automatically updates all indexes
- Can be restored with `restore()`
- Returns `Error::NotFound` if ID doesn't exist

```rust
db.delete(&id)?;
```

### `iter() -> Vec<Value>`

Return all active (non-deleted) documents. Thread-safe (returns cloned values).

```rust
for doc in db.iter() {
    println!("{}", doc);
}
```

### `len() -> usize`

Number of active documents.

### `is_empty() -> bool`

Check if database has no documents.

### `contains(id: &str) -> bool`

Check if a document exists (and is not deleted).

---

## Layer 2: Field Queries

### `find(field: &str, value: &Value) -> Vec<Value>`

Find all documents where `field` equals `value`.

- Uses hash index if available (O(1) per match)
- Falls back to linear scan otherwise

```rust
let active = db.find("status", &json!("active"));
let by_email = db.find("email", &json!("alice@example.com"));
```

### `find_where(field: &str, predicate: F) -> Vec<Value>`

Find documents where the field value matches a custom predicate.

```rust
let seniors = db.find_where("age", |v| v.as_u64().unwrap_or(0) >= 65);
```

### `find_range(field: &str, min: &Value, max: &Value) -> Vec<Value>`

Find documents where `min <= field_value <= max` (inclusive).

- Uses BTree index if available for O(log n) range scan
- Falls back to linear scan

```rust
let mid_range = db.find_range("score", &json!(50), &json!(100));
```

---

## Layer 3: JSON AST Queries

### `query(ast: Value) -> Vec<Value>`

Execute a JSON AST query. The AST is a plain JSON object representing filter conditions.

```rust
let results = db.query(json!({
    "status": {"$eq": "active"},
    "age": {"$gte": 18}
}));
```

### `query_with(ast: Value, opts: QueryOptions) -> Vec<Value>`

Execute a query with sorting, offset, and limit.

```rust
use ndb::{QueryOptions, SortDir};

let results = db.query_with(
    json!({"status": {"$eq": "active"}}),
    QueryOptions {
        sort_by: Some(("age".to_string(), SortDir::Desc)),
        offset: Some(10),
        limit: Some(5),
    },
);
```

### Query Operators

| Operator | Description | Example |
|----------|-------------|---------|
| `$eq` | Equal | `{"field": {"$eq": "value"}}` |
| `$ne` | Not equal | `{"field": {"$ne": "value"}}` |
| `$gt` | Greater than | `{"field": {"$gt": 10}}` |
| `$gte` | Greater than or equal | `{"field": {"$gte": 10}}` |
| `$lt` | Less than | `{"field": {"$lt": 100}}` |
| `$lte` | Less than or equal | `{"field": {"$lte": 100}}` |
| `$in` | In array | `{"field": {"$in": [1, 2, 3]}}` |
| `$nin` | Not in array | `{"field": {"$nin": [1, 2, 3]}}` |
| `$exists` | Field exists (bool) | `{"field": {"$exists": true}}` |

### Implicit `$eq`

If a condition is a plain value (not an object with `$` operators), it's treated as `$eq`:

```rust
// These are equivalent:
db.query(json!({"status": "active"}));
db.query(json!({"status": {"$eq": "active"}}));
```

### Logical Combinators

| Combinator | Description | Example |
|------------|-------------|---------|
| `$and` | All conditions must match | `{"$and": [{...}, {...}]}` |
| `$or` | Any condition must match | `{"$or": [{...}, {...}]}` |
| `$not` | Negate condition | `{"$not": {...}}` |

Combinators can be nested to any depth:

```rust
db.query(json!({
    "$and": [
        {"$or": [
            {"status": {"$eq": "active"}},
            {"status": {"$eq": "pending"}}
        ]},
        {"age": {"$gte": 18}},
        {"$not": {"banned": {"$eq": true}}}
    ]
}));
```

### Dot Notation

Nested fields can be queried using dot notation:

```rust
db.query(json!({
    "user.address.city": {"$eq": "Berlin"}
}));
```

### Array at Top Level = Implicit `$and`

```rust
// These are equivalent:
db.query(json!([{"a": 1}, {"b": 2}]));
db.query(json!({"$and": [{"a": 1}, {"b": 2}]}));
```

---

## Index Management

### `create_index(field: &str) -> Result<()>`

Create a hash index on a field. Scans all existing documents once. O(1) equality lookups.

```rust
db.create_index("email")?;
// Now find("email", ...) uses the index
```

### `create_btree_index(field: &str) -> Result<()>`

Create a BTree index on a field. Enables O(log n) range queries.

```rust
db.create_btree_index("score")?;
// Now find_range("score", min, max) uses the index
```

### `drop_index(field: &str) -> Result<()>`

Drop an index, freeing memory. Returns `Error::IndexError` if no index exists.

```rust
db.drop_index("email")?;
```

### `has_index(field: &str) -> bool`

Check if an index exists for a field.

---

## Compaction & Trash

### `compact() -> Result<()>`

Rewrite the JSONL file to contain only active documents. Archive deleted documents to trash.

- No-op for in-memory databases
- Atomic: uses temp file + rename
- Safe to call periodically

```rust
db.compact()?;
```

### `restore(id: &str) -> Result<()>`

Restore a soft-deleted document. Reads the file to find the last non-deleted version.

```rust
db.delete(&id)?;
db.restore(&id)?;  // Document is back
```

### `deleted_ids() -> Vec<String>`

List IDs of all soft-deleted documents.

### `trash_dir() -> PathBuf`

Get the path to the trash directory.

---

## Persistence

### `flush() -> Result<()>`

Explicitly flush pending writes to disk. Calls `fsync` on the file.

- No-op for in-memory databases

```rust
db.insert(json!({"important": "data"}))?;
db.flush()?;  // Guaranteed on disk
```

### `path() -> &Path`

Get the database file path. Empty path for in-memory databases.

---

## File Buckets

### `bucket(name: &str) -> FileBucket`

Get or create a named file bucket for binary storage.

```rust
let avatars = db.bucket("avatars");
let meta = avatars.store("photo.png", image_data, "image/png")?;
let data = avatars.get(&meta._file)?;
```

See [File Buckets](./file-buckets.md) for full documentation.

---

## Error Handling

All operations return `ndb::Result<T>` which is `std::result::Result<T, ndb::Error>`.

### Error Variants

| Variant | When | Example |
|---------|------|---------|
| `Io` | File system errors | Can't read/write file |
| `Corruption` | Data corruption detected | Malformed data |
| `NotFound` | Document not found | `get("nonexistent")` |
| `InvalidArgument` | Bad input | Restore in in-memory db |
| `Serialization` | JSON parse/serialize errors | Invalid JSON |
| `DatabaseLocked` | Concurrent access conflict | Already locked |
| `IndexError` | Index operation failed | Drop nonexistent index |
| `BucketError` | File bucket error | File not in bucket |

```rust
match db.get(&id) {
    Ok(doc) => println!("{}", doc),
    Err(Error::NotFound { id }) => eprintln!("Not found: {}", id),
    Err(e) => return Err(e.into()),
}
```

---

## Enums

### `Persistence`

```rust
pub enum Persistence {
    Lazy,                        // Default. Flush on explicit call.
    Scheduled(Duration),         // Flush every N seconds.
    Immediate,                   // fsync after every write.
}
```

### `TrashMode`

```rust
pub enum TrashMode {
    Manual,                      // Default. Keep trash forever.
    TTL(Duration),               // Auto-purge after duration.
    Off,                         // Hard delete immediately.
}
```

### `SortDir`

```rust
pub enum SortDir {
    Asc,
    Desc,
}
```

### `QueryOptions`

```rust
pub struct QueryOptions {
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub sort_by: Option<(String, SortDir)>,
}
```
