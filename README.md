# nDB

> Human-readable document database for the AI age.

nDB is an **in-memory document database** with JSON Lines persistence, layered query API, and file bucket support. It runs either embedded in a Rust process or as the standalone multi-DB HTTP service `ndb serve` (port 8323) — the way Node and Electron applications share one database without holding a copy in their own heap.

> **v3.0.0 (2026-11-25) — RAM-first is the only binary engine:**
> - **RAM is the store.** Documents live as `DocVal`/`DocRecord` trees with Hash/BTree indexes; nothing is mmap'ed and no index section is read from disk. The v2 mmap engine (`src/v2_engine.rs`, `src/mmap.rs`, `V2Slot`, `wal_overlay`) is retired and refused loudly — convert with the preserved transition binary (`ndb migrate <path> --ram`) first.
> - **Durability** is an op-level WAL (`wal.gen_<N>.ndb`) whose batches end in a `Commit` frame, written with per-database group commit. RAM is published only after the fsync; a failed fsync publishes nothing and poisons the DB (`503 db_poisoned`). The memory budget answers `507 memory_limit_exceeded` before the WAL is touched.
> - **Compaction** is non-blocking and persists `pending_wal_gen` in the manifest before the writer rotates, so a stalled compaction is recoverable and a retry reuses the pending generation.
> - **Standalone object & query contract (Phase 4A)** — `ndb serve` alone is a useful database: `POST /v2/dbs/{name}/search` and `query.text` (case-insensitive lexical AND-terms + `"quoted phrases"`, no ranking/stemming/vectors), multi-key `sort_keys`, opaque `cursor` pagination, a `max_scan_docs` budget (`422 scan_budget_exceeded`), per-page `scanned_docs`/`used_indexes`/`next_cursor`, `GET|PUT /schema` field mapping (`hash`/`ordered`/`text`/`project`), an opt-in classification profile (`POST /profile/classification`) and `seq`-pinned partial retrieval (`POST /doc/{id}/peek`).
> - **Search ownership** — advanced full-text (BM25, vectors, hybrid, rerank) is **nVDB's** job; nDB's `/search` is a correct, honest scan that keeps the database useful with nVDB stopped. `mcp_server` coordinates both through `src/lib/ndb-query-facade.js`.
> - **Node.js** — the `napi/` crate and its `.node` prebuilts remain gone (v2.0.0); consumers use `ndb serve` over HTTP.
> - Stray `_index_*.fti` files left in a folder by older versions are ignored with a warning, so existing databases mount unchanged.

> **⚠️ Database-as-a-Folder (unchanged since v3)**
> nDB uses a **Database-as-a-Folder** layout: one directory per database containing
> - **v3 (default)**: `manifest.json` (generation/lineage authority), `base.gen_<N>.ndb` (snapshot), `wal.gen_<N>.ndb` (op-level WAL) — pass the folder to `Database::open`
> - **v1**: `data.jsonl` (the append-only document store)
> - either way: `_files/` (managed binary buckets with SHA-256 deduplication, created implicitly), `_trash/` (soft-deleted documents and files), `meta.json` (schema/bucket metadata — written by the CLI/migration, **not yet enforced** by the core)
>
> This removes the need for upper-layer management wrappers (like the deprecated nGDB). nDB natively handles **Delta patch operations** (e.g. `array_push`) for large objects and **bucket garbage collection**. Schema enforcement and nURI link-type validation remain unimplemented — the core reads `meta.json`'s `buckets` policy block (kind, lifecycle, TTLs) but ignores the `schemas` block.

## What's New in v3.0.0
- **RAM-first storage** — authoritative documents in RAM (`DocVal`), op-level WAL with a `Commit` boundary and group commit, bounded change ring, non-blocking compaction, pinned snapshots, memory governance. The v2 mmap engine is gone.
- **Standalone query contract** — lexical `/search` + `query.text`, `sort_keys`, cursor pagination, scan budgets, schema/field mapping, classification profile, partial `peek` retrieval. nDB stays fully usable with nVDB stopped.
- **Standalone multi-DB service** — one `ndb serve` process mounts independent database folders by name (`--db name=path`, `--dir <root>`, `--config`), with per-DB v3 storage and group-commit durability.
- **Zero NAPI** — the `napi/` crate and committed Windows prebuilts are gone (v2.0.0); Node consumers use the HTTP API, which keeps one copy of the data in one Rust process instead of one copy per Node process.

Release history: **v2.0.0** (2026-11-23) removed the Node binding and the in-crate inverted index; **v3.0.0** (2026-11-25) retired the v2 mmap engine and made RAM-first the default.

## Features

- **O(1) CRUD** — All core operations are HashMap lookups
- **RAM-first binary storage** — authoritative in-RAM documents, op-level WAL with a `Commit` boundary, group commit, non-blocking compaction (v3; the only binary engine)
- **Database-as-a-Folder** — Encapsulated `meta.json`, `manifest.json`/`base.gen_*`/`wal.gen_*` (v3) or `data.jsonl` (v1), plus `_files` and `_trash` directories.
- **Atomic Delta Updates** — Native `array_push` and patching for massive documents to avoid O(N²) I/O bloat.
- **3-layer query API + standalone query contract** — From simple lookups to complex JSON AST queries, multi-key sort, cursor pagination, lexical `text` search and scan budgets
- **Opt-in indexes** — Hash indexes for equality, BTree indexes for ranges
- **File buckets** — Binary storage with SHA-256 deduplication
- **Soft delete & trash** — Recoverable deletes with compaction
- **Multi-DB HTTP service** — `ndb serve`: one resident Rust process, many named databases, HTTP/1.1 + keep-alive
- **Zero dependencies** — Minimal Rust crate, no external crypto/DB libs

## Quick Start

### Rust

`ndb init <path>` creates a **v3 RAM-first** folder; `Database::open` detects the backend (v3 manifest, v1 `data.jsonl`, or an empty folder as v1).

```rust
use ndb::{Database, Persistence};
use serde_json::json;

let db = Database::open("mydata")?
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

Full HTTP contract: `docs/contract_http-api.md` (design rationale: `docs/ndb-serve-design.md`).
