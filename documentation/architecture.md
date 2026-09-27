# nDB Architecture

> Internal design and data flow of the nDB document database.

---

## Overview

**Three interfaces.** nDB is consumed three ways: the Rust crate (`Database::open`), the Node.js bindings (napi, `napi/`), and the **HTTP daemon** (`ndb serve`, design: `docs/ndb-serve-design.md`) — a resident process that owns the database in memory and serves queries to any client over HTTP. The daemon exists for large shared databases (multi-GB) where N apps must not each hold the data in their own heap: data and computation stay in Rust, only results cross the wire.

**Database-as-a-Folder.** nDB's intended and production structure is one **directory per database**. The convention used by the primary consumer (LLM-Gateway-Chat) and produced by the CLI is:

```
my-app/
├── data.jsonl     # The append-only document store (this file is passed to Database::open)
├── _files/        # Managed binary buckets (SHA-256 deduplication)
├── _trash/        # Soft-deleted documents and files
└── meta.json      # Schema/bucket metadata — written by the CLI/migration; the core reads the `buckets` policy block at open (schema validation still not implemented)
```

**Important nuance:** the Rust core's `Database::open(path)` takes the **`data.jsonl` file itself**, not the folder. The library derives the folder as the file's parent and creates `_files/` and `_trash/` as siblings of the `.jsonl`. So a "folder database" is a *container convention* imposed by the caller (or the CLI), not something `open()` sets up for you. `meta.json` is present in real folders; the core reads the `buckets` policy block at open (see below) but does not write the file, and schema validation is not implemented.

### Layout convention: database-as-a-folder

Every database lives in its own directory: the `.jsonl` inside it, with `_files/` and `_trash/` alongside. This is the layout used by all consumers (LLM-Gateway-Chat, mcp_server) and produced by `ndb init`. A bare `.jsonl` placed outside a dedicated folder will still get a sibling `_trash/` (and `_files/` if buckets are used) — the library creates them next to the file — but new databases should always be created as folders so documents, buckets, trash, and future `meta.json` stay encapsulated in one directory.

Every document lives in a `HashMap<String, Value>` at runtime, providing O(1) lookups by `_id`. Persistence is achieved through an append-only JSON Lines file.

```
┌─────────────────────────────────────────────────┐
│                   Application                    │
├─────────┬──────────┬──────────┬─────────────────┤
│ Layer 1 │ Layer 2  │ Layer 3  │  File Buckets   │
│  CRUD   │  Find    │  Query   │  Binary Store   │
│  O(1)   │  Field   │  AST     │  SHA-256 Dedup  │
├─────────┴──────────┴──────────┴─────────────────┤
│              In-Memory HashMap                   │
│         RwLock<HashMap<String, Value>>           │
├─────────────────────────────────────────────────┤
│           JSON Lines Persistence                 │
│         Append-Only + Compaction                 │
└─────────────────────────────────────────────────┘
```
`Database::open("my-app/data.jsonl")` → in-memory store backed by `my-app/data.jsonl`, with buckets in `my-app/_files/` and trash in `my-app/_trash/`.

**Deployment (submodule workflow).** Consumers embed nDB as a git submodule. Per machine, after pulling:

```
node setup.js        # builds the CLI (ndb, incl. `ndb serve`) → bin/ndb[.exe]
node napi/setup.js   # builds the Node bindings → napi/*.node
```

`bin/` and `target/` are per-machine artifacts (gitignored). The committed `napi/*.win32-x64-msvc.node` files are prebuilts for Windows x64; other platforms build via `napi/setup.js`.

---

## Concurrency Model

**Single-writer, multi-reader** using `parking_lot` primitives:

| Operation | Lock | Behavior |
|-----------|------|----------|
| `insert()` | `writer` Mutex | Exclusive write access |
| `update()` | `writer` Mutex | Exclusive write access |
| `delete()` | `writer` Mutex | Exclusive write access |
| `array_push()` | `writer` Mutex | Exclusive write access |
| `set()` | `writer` Mutex | Exclusive write access |
| `remove()` | `writer` Mutex | Exclusive write access |
| `get()` | `docs` RwLock (read) | Concurrent reads |
| `find()` | `docs` RwLock (read) | Concurrent reads |
| `query()` | `docs` RwLock (read) | Concurrent reads |
| `iter()` | `docs` RwLock (read) | Concurrent reads |

Multiple threads can read simultaneously. Only one thread can write at a time. Writes do not block reads (RwLock allows concurrent readers while writer is waiting).

---

## Storage Format

### JSON Lines (JSONL)

The database file is a sequence of JSON objects, one per line:

```
{"_meta":{"version":1,"created":1711553200}}
{"_id":"V1StGXR8Z5jdHi6B","name":"Alice","age":30}
{"_id":"k8Tm2pQw4xNvRj7L","name":"Bob","age":25}
{"_id":"V1StGXR8Z5jdHi6B","name":"Alice Smith","age":31}
{"_id":"V1StGXR8Z5jdHi6B","_op":"set","path":"age","value":32}
{"_id":"V1StGXR8Z5jdHi6B","_op":"remove","path":"name"}
{"_id":"k8Tm2pQw4xNvRj7L","_deleted":1711553300}
```

### Rules

1. **Line 1** is always the `_meta` header with version and creation timestamp.
2. Each subsequent line is a complete JSON object.
3. **Last write wins**: if multiple lines have the same `_id`, the last one is the current version.
4. **Tombstones**: a line with `_deleted` marks a document as deleted.
5. **Delta patches**: lines with `_op` are patches applied on top of the base document during replay.
6. **Append-only**: writes only append to the end of the file.

### Crash Recovery

On load, `read_all()` parses each line individually. Malformed or truncated lines (from power loss during write) are **skipped with a warning** rather than causing a fatal error. This means:

- All data written *before* the crash is preserved.
- Only the incomplete last write is lost.
- The database opens successfully after a crash.

---

## Document IDs

### Format

- **16 characters**, base62 alphabet: `[a-zA-Z0-9]`
- Collision space: 62^16 ≈ 4.7 × 10^28
- Generated by PRNG (`fastrand`), checked for uniqueness against existing HashMap
- Optional prefix support: `prefix_V1StGXR8Z5jdHi6B`

### Generation

```rust
// Standard ID
let id = db.insert(doc)?;  // e.g. "V1StGXR8Z5jdHi6B"

// Prefixed ID
let id = db.insert_with_prefix("user", doc)?;  // e.g. "user_V1StGXR8Z5jdHi6B"
```

Uniqueness is guaranteed by checking against the in-memory HashMap. On collision (astronomically unlikely), the generator retries up to 10 times before panicking.

---

## Persistence Modes

| Mode | Behavior | Use Case |
|------|----------|----------|
| `Lazy` (default) | Buffer in OS cache, flush on explicit `flush()` or drop | Fastest, for caches/temp data |
| `Scheduled(Duration)` | Flush every N seconds | Balanced, for most applications |
| `Immediate` | `fsync` after every write | Maximum safety, for critical data |

```rust
let db = Database::open("data.jsonl")?
    .with_persistence(Persistence::Immediate);
```

---

## Compaction

Over time, the JSONL file accumulates:
- Superseded versions (old versions of updated documents)
- Tombstones (deleted document markers)

Compaction rewrites the file to contain only active documents:

1. Acquire writer lock
2. Close file handle
3. Read all active documents from HashMap
4. Write to a temporary file
5. Atomic rename (temp → real file)
6. Archive deleted documents to `_trash/docs/`

```rust
db.compact()?;  // Rewrites file, archives trash
```

---

## Trash System

### Document Trash

Soft-deleted documents are archived during compaction:

```
data/
├── data.jsonl           # Active documents only
├── _trash/
│   ├── docs/
│   │   └── data.jsonl   # Archived deleted documents
│   └── files/
│       └── avatars/     # Archived deleted files
└── _files/
    └── avatars/         # Active file bucket
```

### Restore

```rust
db.delete(&id)?;      // Soft delete
db.restore(&id)?;     // Bring it back
```

Restore reads the file to find the last non-deleted version of the document and re-inserts it. Files recorded in the trash entry under `_trashed_files` are returned to their buckets first; a file that cannot be returned is reported and the document is restored anyway, so a dangling reference stays visible instead of being silently re-declared as restored.

### Failure Discipline

Two kinds of failure, two contracts:

- **A precondition of the operation the caller asked for** fails — the call returns an error and the operation does not happen. `insert`, `insert_with_prefix` and `update` reject a non-object document with `InvalidArgument` instead of panicking on caller-supplied shape; `delete` treats the trash record as a precondition, so a document is never reported deleted when the copy that makes it restorable could not be written.
- **Ancillary cleanup** fails — the primary result stands and the failure is reported on stderr with the operation named (`ndb: suppressed failure: ...`). A file that cannot be moved to trash, a bucket that cannot be swept, a flush at shutdown. The count `gc_buckets()` returns covers only files that actually moved.

Nothing is swallowed. There is no bare `let _ =` on a `Result` anywhere in the library; the sink and its rationale live in `src/report.rs`.

### Reliability at the JS boundary

In Rust a `Result` is a returned value: it cannot crash anything, and the caller cannot proceed past it without looking at it. That stays the library's contract.

At the napi boundary an `Err` becomes a **JavaScript exception** — the established contract, and a fine one: a caught exception lets an application report a failed operation and carry on. What it does *not* tolerate is a failure that cannot be caught at all.

So the boundary makes four guarantees, and they are about crash-and-data, not about avoiding exceptions:

- **No process abort.** Panics abort outright and cannot be caught on either side, so caller-supplied shape is validated rather than unwrapped — a non-object document raises a catchable error instead of taking the host down. A poisoned lock is reported for the same reason.
- **A delete is never reported as reversible when it is not.** The restorable copy is written before anything destructive, so an unrecordable delete is refused rather than reported as success. This is delete-specific; see the caveat below.
- **Cleanup never fails the operation.** Moving files to trash, sweeping buckets, purging trash and flushing at shutdown report failures on stderr (`ndb: suppressed failure: ...`) and leave the primary result intact. `gc_buckets()` returns a count of files that actually moved.
- **A failed journal write changes nothing.** Every mutating operation — `insert`, `update`, the delta ops `set`/`remove`/`array_push`, and `delete` — appends to the journal *before* any in-memory, index, ref-counter or file-bucket state is touched. If the append fails, the call raises and the document, indexes, ref counts, live blobs and the journal itself are exactly as they were; the operation is retryable. What happens after a successful append is infallible bookkeeping or reported cleanup.

The asymmetry is deliberate: protecting the data that exists outranks every reporting preference.

#### The ordering contract

The write paths run prepare → journal → commit. The delete path adds a precondition before all of it (the restorable copy), and file moves run last inside the commit — a file that fails to move after the commit is under-cleaning: named, reported, and swept by `gc_buckets` later, never a silent loss. The contract is pinned by fault-injected tests (`tests/phase9_failure_discipline_tests.rs`): the journal path is replaced by a directory so the append-open fails, and the tests assert that document, indexes, ref counters, blobs and the replayed journal are all untouched — and that the same operation succeeds whole on retry.

This was the resolution of issue #7: before the reorder, `update` and the delta ops mutated memory, indexes and ref counters *before* the append, so a failed write could trash live media while the document stayed old.

### Trash Modes

| Mode | Behavior |
|------|----------|
| `Manual` (default) | Keep trash until manual purge |
| `TTL(Duration)` | Auto-purge after duration |
| `Off` | No trash, hard delete only |

---

## Indexes

Indexes are **opt-in** secondary data structures that accelerate queries.

### Hash Index

- O(1) equality lookups
- Used by `find(field, value)` when available
- Memory: linear in number of unique values

```rust
db.create_index("email")?;
let results = db.find("email", &json!("alice@example.com"));
```

### BTree Index

- O(log n) lookups + range queries
- Used by `find_range(field, min, max)`
- Numbers are zero-padded for correct ordering

```rust
db.create_btree_index("score")?;
let results = db.find_range("score", &json!(50), &json!(100));
```

### Index Maintenance

Indexes are automatically updated on `insert()`, `update()`, and `delete()`. No manual reindexing needed.

```rust
db.drop_index("email")?;  // Free memory
```

---

## File Layout

The canonical folder-per-database layout (as used by LLM-Gateway-Chat and created by `ndb init`):

```
my-app/
├── data.jsonl              # Document store (JSON Lines) — passed to Database::open
├── meta.json               # Engine metadata (version, buckets, schemas). Written by CLI/migration; the core enforces the `buckets` policy block (onDocumentDelete, ttl_seconds) — schemas are ignored for now.
├── _files/                 # File buckets root (created implicitly by bucket operations)
│   ├── avatars/            # Named bucket "avatars"
│   │   ├── a1b2c3d4.png    # Stored by SHA-256 hash prefix
│   │   └── e5f6g7h8.jpg
│   └── attachments/
│       └── i9j0k1l2.pdf
└── _trash/                 # Trash root
    ├── docs/
    │   └── data.jsonl      # Archived deleted documents
    └── files/
        └── avatars/        # Archived deleted files
            └── a1b2c3d4.png
```

**CLI alignment:** the CLI (`ndb init`, `info`, `verify`, `export`, …) uses the same layout as the library and the primary consumer — `data.jsonl` (doc store), `_files/` (buckets), `_trash/` (trash), plus `meta.json`. A folder created by `ndb init` can be opened directly with `Database::open("mydb/data.jsonl")`.

## v3 Breaking Changes
- **Atomic Deltas:** Three patch operations (`array_push`, `set`, `remove`) enable O(1) file writes for field-level edits without rewriting entire documents.
- **File Buckets & nURI:** `avatars:a1b2c3.png` strings are deduplicated natively.

## Delta Patch System

### The Hammer vs The Scalpel

nDB supports two distinct write patterns:

1. **Full Replacements (Hammer):** `db.update(id, fullDoc)` appends the entire document to the JSONL. Simple and pure for small documents.

2. **Delta Patches (Scalpel):** `db.set()`, `db.remove()`, and `db.arrayPush()` append tiny instructions instead of full documents. Essential for large documents (e.g. 3MB conversation objects).

### Patch Format

Patches are stored as regular JSONL lines with an `_op` field:

```jsonl
{"_id":"chat_123","_op":"array_push","field":"messages","value":{"text":"hi"}}
{"_id":"chat_123","_op":"set","path":"messages.0.text","value":"hello"}
{"_id":"chat_123","_op":"remove","path":"temporary_data"}
```

### Replay Engine

On `Database::open()`, all JSONL lines are processed sequentially:

1. **Full doc** → inserted into HashMap (last write wins)
2. **Tombstone** (`_deleted`) → removed from HashMap
3. **`array_push` patch** → pushes value to the specified array field
4. **`set` patch** → walks the dot-path and sets the value
5. **`remove` patch** → walks the dot-path and removes the target

If a patch's path can't be resolved (missing field, out-of-bounds index), it is **silently skipped**. No data corruption is possible.

### Compaction

`db.compact()` bakes all patches into fresh base documents. The JSONL is rewritten containing only the current in-memory state of each document. All patches are eliminated.

### Full Update Absorption

If a `db.update()` (full replacement) occurs after patches, it overwrites the entire document. Any subsequent patches apply on top of the new base. This is correct because the replay engine processes entries in order.
