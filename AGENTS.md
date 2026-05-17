# AGENTS.md — nDB Project Overview

> High-level context for AI coding agents working on this project.
> This document describes intent, architecture, and current state.
> It does not depend on file layout and is not a usage guide.

---

## Core Development Maxims

- **Priorities:** Reliability > Performance > Everything else.
- **LLM-Native Codebase:** Code readability and structure for *humans* is a non-goal. The code will not be maintained by humans. Optimize for the most efficient structure an LLM can understand. Do not rely on conventional coding habits for either Rust or JavaScript.
- **Minimal Dependencies:** If the standard library (Rust `std` / Node.js built-ins) can do it, use it. Avoid external packages. Evaluate per-case whether a dependency is truly necessary — in Rust this means crates beyond `serde`, `parking_lot`, `fastrand`, and `thiserror`; in Node.js this means no npm packages unless unavoidable.
- **Vanilla JavaScript:** No TypeScript anywhere in the Node.js layer. Code stays as close to the bare platform as possible for easy optimization and debugging. `.d.ts` files are generated strictly for LLM/editor context, not used at runtime.
- **Fail Fast, Always:** No defensive coding. No mock data. No fallback defaults. No silencing `try/catch` or swallowing `Result` errors. No optional chaining (`?.`) for required values. Configuration must be explicit — missing required config must throw immediately at startup. When something breaks, let it crash and fix the root cause.
- **Collaborative Development:** The human user is a partner, not just a reviewer. When facing architectural decisions, trade-offs, or uncertain paths, pause and ask for input. Explain the options clearly. The user's domain knowledge and preferences are valuable — include them in the loop. Avoid long silent stretches of trial-and-error; converse, don't just execute.

---

## What nDB Is

nDB is an **embeddable in-memory document database** built in Rust with native Node.js bindings. It stores JSON documents in a `HashMap` for O(1) lookups and persists them to an append-only JSON Lines file on disk. It is designed for AI-age applications — chat apps, agents, Electron tools — where you need fast local storage without external database servers.

## Core Design

- **Single-writer, multi-reader** concurrency. One thread writes at a time; many threads can read simultaneously.
- **Append-only JSON Lines** persistence. Every write appends a line. On load, the file is replayed: last write wins, tombstones mark deletions, delta patches are applied in order.
- **Database-as-a-Folder.** Opening a path creates `meta.json` (schema/config), `data.jsonl` (documents), `_trash/` (soft-deleted items), and `_files/` (binary storage).
- **Zero dependencies beyond the Rust standard library plus `serde`, `serde_json`, `parking_lot`, `fastrand`, and `thiserror`.** No external crypto, no external DB engines. SHA-256 for file deduplication is hand-rolled.

## Query Layers

1. **Layer 1 — O(1) CRUD:** `insert`, `get`, `update`, `delete`, `array_push`. Direct HashMap operations.
2. **Layer 2 — Field Queries:** `find(field, value)`, `find_where()`, `find_range()`. Accelerated by opt-in hash and BTree indexes.
3. **Layer 3 — JSON AST Queries:** MongoDB-style filter objects with `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`, `$in`, `$nin`, `$exists`, `$and`, `$or`, `$not`, and dot-notation nested field access.

## File Buckets

Named binary storage with **SHA-256 content-hash deduplication**. Identical files are stored once. Files are referenced by compact `bucket:hash.ext` strings (nURI format). Soft-delete moves files to `_trash/files/` rather than removing them. Garbage collection (`gc_buckets`) sweeps unreferenced files to trash. Targeted release (`release_file`) checks all in-memory documents before trashing a specific file.

## Atomic Delta Updates

For massive documents (e.g., 3 MB conversation objects), replacing the entire document on every change is catastrophic for I/O. nDB supports writing tiny delta operations to the append-only log:

```json
{"_id": "chat_123", "_op": "array_push", "field": "messages", "value": {...}}
```

On load, patches are replayed in memory. On compaction, patches are baked into a fresh base document.

## Node.js Bindings

Powered by **napi-rs**. The `napi/` crate wraps the Rust `Database` type and exposes all operations to JavaScript. Async tasks handle `query`, `queryWith`, `compact`, and `exportSnapshot`. File bucket operations (`storeFile`, `getFile`, `deleteFile`, `releaseFile`, `gcBuckets`) are exposed directly.

## Current Implementation Status

| Feature | State |
|---|---|
| Core CRUD + persistence | Complete |
| 3-layer query API | Complete |
| Hash + BTree indexes | Complete |
| Database-as-a-Folder | Complete |
| File buckets with SHA-256 dedup | Complete |
| `array_push` delta (Rust core) | Complete |
| `release_file` + `gc_buckets` (Rust + NAPI) | Complete |
| Delta updates in Node.js backend | Not yet wired |
| Schema validation from `meta.json` | Not implemented |
| nURI `link` type enforcement | Not implemented |
| TTL-based trash purging | Not implemented |
| Bucket migration script for legacy data | Not implemented |

## What nDB Is Not

- Not a network database. It has no built-in client-server protocol.
- Not a relational database. No joins, no SQL.
- Not designed for terabyte-scale datasets. Everything lives in memory; persistence is for durability, not paging.
- Not a replacement for PostgreSQL or MongoDB in multi-user server deployments.

## Key Concepts for Agents

- **`_id`** — 16-character base62 NanoID, optionally prefixed (`conv_V1StGXR8Z5jdHi6B`). Generated client-side, collision-checked against the in-memory set.
- **Tombstone** — A JSONL line with `_deleted` timestamp. Marks a document as deleted without removing history.
- **Compaction** — Rewrites the JSONL file to contain only active documents, archiving tombstones to `_trash/docs/`.
- **FileRef** — The `bucket:hash.ext` compact string that links documents to binary files in buckets.
- **nURI** — The planned universal link format (`bucket:hash.ext` for internal, `https://...` for external). Not yet enforced by the schema validator.
