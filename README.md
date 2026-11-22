# nDB

> Human-readable document database for the AI age.

nDB is an **in-memory document database** with JSON Lines persistence, layered query API, and file bucket support. It runs either embedded in a Rust process or as the standalone multi-DB HTTP service `ndb serve` (port 8323) — the way Node and Electron applications share one database without holding a copy in their own heap.

> **v2.0.0 (2026-11-23) — breaking:**
> - The `napi/` crate and its `.node` prebuilts are **gone**. There is no native Node binding; consumers talk to `ndb serve` over HTTP (`mcp_server` and `LLM-Gateway-Chat` already do).
> - The in-crate full-text search engine (`src/search.rs`, `/search`, `text` indexes) is **gone**. Full-text, lexical, vector and hybrid search is owned by **nVDB**; nDB keeps structured query/filter/sort/projection/pagination, typed Hash/BTree indexes, v2 binary/mmap storage with group commit, and the multi-DB HTTP service.
> - Stray `_index_*.fti` files left in a folder by older versions are ignored with a warning, so existing databases mount unchanged.


> **⚠️ BREAKING CHANGE (v3 Architecture)**
> nDB uses a **Database-as-a-Folder** layout: one directory per database containing
> - `data.jsonl` (the append-only document store — this is the file you pass to `Database::open`)
> - `_files/` (managed binary buckets with SHA-256 deduplication, created implicitly)
> - `_trash/` (soft-deleted documents and files)
> - `meta.json` (schema/bucket metadata — written by the CLI/migration, **not yet enforced** by the core)
> 
> This removes the need for upper-layer management wrappers (like the deprecated nGDB). nDB natively handles **Delta patch operations** (e.g. `array_push`) for large objects and **bucket garbage collection**. Schema enforcement and nURI link-type validation remain unimplemented — the core reads `meta.json`'s `buckets` policy block (kind, lifecycle, TTLs) but ignores the `schemas` block.

## What's New in v2.0.0
- **Standalone multi-DB service** — one `ndb serve` process mounts independent database folders by name (`--db name=path`, `--dir <root>`, `--config`), with per-DB v2 binary/mmap storage and group-commit durability.
- **Zero NAPI** — the `napi/` crate and committed Windows prebuilts were removed; Node consumers use the HTTP API, which keeps one copy of the data in one Rust process instead of one copy per Node process.
- **Full-text search moved out** — the in-crate inverted index was deleted (zero production callers); `nVDB` owns all full-text, lexical, vector and hybrid search. Structured query, filters, sort, projection, pagination, typed Hash/BTree indexes and file buckets are unchanged.

## Features

- **O(1) CRUD** — All core operations are HashMap lookups
- **Database-as-a-Folder** — Encapsulated `meta.json`, `data.jsonl`, `_files`, and `_trash` directories.
- **Atomic Delta Updates** — Native `array_push` and patching for massive documents to avoid O(N²) I/O bloat.
- **3-layer query API** — From simple lookups to complex JSON AST queries
- **Opt-in indexes** — Hash indexes for equality, BTree indexes for ranges
- **File buckets** — Binary storage with SHA-256 deduplication
- **Soft delete & trash** — Recoverable deletes with compaction
- **Multi-DB HTTP service** — `ndb serve`: one resident Rust process, many named databases, HTTP/1.1 + keep-alive
- **Zero dependencies** — Minimal Rust crate, no external crypto/DB libs

## Quick Start

### Rust

```rust
use ndb::{Database, Persistence};
use serde_json::json;

let db = Database::open("mydata/data.jsonl")?
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

### Node.js (HTTP)

The native binding was removed in v2.0.0. Run the daemon once and talk to it from any app:

```bash
ndb serve --db notes=D:\data\notes --bind 127.0.0.1 --port 8323
```

```js
const base = 'http://127.0.0.1:8323/v2/dbs/notes';

const id = await fetch(`${base}/doc`, {
  method: 'PUT',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ name: 'Alice', age: 30 }),
}).then(r => r.json()).then(b => b.id);

const { results } = await fetch(`${base}/query`, {
  method: 'POST',
  headers: { 'content-type': 'application/json' },
  body: JSON.stringify({ filter: { age: { $gte: 25 } }, fields: ['name'] }),
}).then(r => r.json());

await fetch(`${base}/doc/${id}`, { method: 'DELETE' });
```

Full contract: `docs/ndb-serve-design.md`.
