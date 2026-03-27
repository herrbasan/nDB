# nDB

> Human-readable document database for the AI age.

nDB is an **in-memory document database** with JSON Lines persistence, layered query API, and and file bucket support. Part of the [nGDB](https://github.com/nickel-org/ngdb) platform ecosystem.

## Features

- **O(1) CRUD** — All core operations are HashMap lookups
- **JSON Lines storage** — Human-readable, append-only, crash-safe
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
const { Database } = require('@ngdb/ndb');

const db = new Database('./mydata.jsonl');

const id = db.insert({ name: 'Alice', age: 30 });
const doc = db.get(id);
const results = db.query({ age: { $gte: 25 } });

db.delete(id);
