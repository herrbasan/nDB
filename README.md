# nDB

> Human-readable document database for the AI age.

nDB is an **in-memory document database** with JSON Lines persistence, layered query API, and file bucket support. Standalone embeddable database for Node.js and Electron applications.


> **⚠️ BREAKING CHANGE (v3 Architecture)**
> nDB has transitioned to a **Database-as-a-Folder** architecture. It is no longer just a flat `.jsonl` file.
> Opening a database now treats the path as a directory containing:
> - `meta.json` (Schema and bucket definitions)
> - `data.jsonl` (The append-only document store)
> - `_trash/` (Soft-deleted documents and files)
> - `_files/` (Managed binary buckets with SHA-256 deduplication)
> 
> This removes the need for upper-layer management wrappers (like the deprecated nGDB). nDB now natively handles **Delta patch operations** (e.g. `array_push`) for large objects, **opt-in schema enforcement**, **nURI Links** (`bucket:hash.ext`), and **bucket garbage collection**.

## What's New in v1.2.0 (Non-Breaking)
- **Background Trash TTL**: Added the ability to define a TTL (Time-To-Live) for trashed files and documents, with a non-blocking background thread that automatically cleans up expired trash.
- **Garbage Collection API**: Added new `gcBuckets()` (Node.js) / `gc_buckets()` (Rust) APIs to scan file buckets and automatically trash unreferenced files.
- **Opt-in Compatibility**: These additions are completely backward-compatible. Users can opt in to the background GC via `trash_ttl` and `trash_purge_interval` options in `Database.open()`.

## Features

- **O(1) CRUD** — All core operations are HashMap lookups
- **Database-as-a-Folder** — Encapsulated `meta.json`, `data.jsonl`, `_files`, and `_trash` directories.
- **Atomic Delta Updates** — Native `array_push` and patching for massive documents to avoid O(N²) I/O bloat.
- **3-layer query API** — From simple lookups to complex JSON AST queries
- **Opt-in indexes** — Hash indexes for equality, BTree indexes for ranges
- **File buckets** — Binary storage with SHA-256 deduplication
- **Soft delete & trash** — Recoverable deletes with compaction
- **Node.js native bindings** — napi-rs powered, zero-copy where possible
- **Zero dependencies** — Minimal Rust crate, no external crypto/DB libs

## Quick Start

### Rust

```rust
use ndb::{Database, Persistence};
use serde_json::json;

let db = Database::open("mydata.jsonl")?
    .with_persistence(Persistence::Immediate);

// Insert
let id = db.insert(json!({"name": "Alice", "age": 30}))?;

// Get (O(1))
let doc = db.get(&id)?;

// Query
let results = db.query(json!({"age": {"$gte": 25}}));

// Delete & restore
db.delete(&id)?;
db.restore(&id)?;
```

### Node.js
```js
const { Database } = require('ndb');

const db = new Database('./mydata.jsonl');

const id = db.insert({ name: 'Alice', age: 30 });
const doc = db.get(id);
const results = db.query({ age: { $gte: 25 } });

db.delete(id);
