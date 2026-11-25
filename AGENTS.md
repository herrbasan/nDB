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

nDB is an **embeddable in-memory document database** built in Rust. It stores JSON documents in a `HashMap` for O(1) lookups and persists them to an append-only JSON Lines file (v1) or a binary/mmap generation pair (v2). Since **v2.0.0** it is also the **standalone multi-DB HTTP service `ndb serve`** — the only non-Rust interface. The napi Node binding crate was removed in v2.0.0 (no `.node` prebuilts, zero NAPI); `mcp_server` and `LLM-Gateway-Chat` talk to `ndb serve`. Full-text, lexical, vector and hybrid search belong to `nVDB`, not here.

## Core Design

- **Single-writer, multi-reader** concurrency. One thread writes at a time; many threads can read simultaneously.
- **Append-only JSON Lines** persistence. Every write appends a line — payload + `\n` in one buffered `write_all`. On load the journal is streamed line by line; last write wins, tombstones mark deletions, delta patches are applied in order. A torn *final* line is truncated at the last good byte offset (with a warning); **mid-journal corruption fails loudly** (`Error::Corruption` naming the line) and is recovered with `ndb repair <path>`, which quarantines the file to `<file>.corrupt.<ts>.bak` and truncates at the last good line.
- **Durable lineage + change feed.** `_meta` carries `source_epoch_uuid`, `last_seq`, `min_available_seq`; every committed line carries its `_seq`. `lineage()`, `snapshot_at()` and `changes_since(seq)` expose the journal as a replayable feed under the writer lock. `_seq` is engine-owned and stripped before any document reaches memory, so no read API returns it. `seq` never resets — not on compaction, not on restart.
- **Database-as-a-Folder.** One directory per database: `data.jsonl` (documents — the file passed to `open()`), `_files/` (binary storage, created implicitly), `_trash/` (soft-deleted items), and `meta.json` (bucket declarations/policies — written by the CLI/migration, **read by the core at open**: `kind`, `onDocumentDelete`, `ttl_seconds`, `reserved_ttl_seconds`; the `schemas` block is still ignored).
- **Zero dependencies beyond the Rust standard library plus `serde`, `serde_json`, `parking_lot`, `fastrand`, and `thiserror`.** No external crypto, no external DB engines. SHA-256 for file deduplication is hand-rolled.

## Storage Backends (v1 JSONL / v2 binary)

`Database::open` detects the backend from the folder: `manifest.json` → v2,
`data.jsonl` → v1, empty → v1 (or v2 when `OpenOptions::format == V2`). The
public API is identical on both.

- **v2 files**: `manifest.json` (sole generation/lineage authority),
  `base.gen_<N>.ndb` (immutable mmap'ed snapshot with persisted Hash/BTree
  index sections), `wal.gen_<N>.ndb` (CRC-32 framed append log). Every parser
  validates its CRC and structure; a torn WAL tail is truncated at open, any
  other corruption fails loud.
- **In-memory**: the mapping is the store. `V2State` holds `Arc<MmapBase>`, a
  `superseded` bitset (one bit per base slot) and a `wal_overlay` of
  post-compaction writes only — never a per-document map and never heap `Value`
  trees. A cold open allocates nothing per document; reads binary-search the
  mapped directory. Persisted indexes are probed in place (binary search over
  encoded keys, `&[u32]` slot views); only the small WAL overlay carries an
  in-memory index.
- **Queries**: filters and sort keys are read from document bytes
  (`src/v2_json_scan.rs`); `sort_by`+`limit` is a bounded top-K and only the
  returned page (or its requested fields) is materialized. `/query` without
  `fields` streams stored bytes into the response.
- **Writes**: per-database leader/follower group commit coalesces concurrent
  mutations into one `append_batch_and_sync`; RAM (overlay, bitset, indexes,
  change ring, `last_seq`) is published only after the fsync, and a failed fsync
  publishes nothing.
- **Feed**: the RAM change ring is capped at 1024 events; older cursors are
  answered by streaming the WAL from disk.
- **Compaction** is non-blocking: freeze under a short lock (manifest + `Arc` +
  bitset + overlay — a few KB, not the document set), write the new base with a
  mirror WAL capturing concurrent writes, then swap the manifest and the live
  mapping in a second short lock.

Engine: `src/v2_engine.rs`; on-disk format: `src/v2_format.rs`, raw-JSON scan:
`src/v2_json_scan.rs`, mapping: `src/mmap.rs`, checksums: `src/crc32.rs`.
CLI: `ndb init --v2`, `migrate`, `fsck`, `repair`, `export --jsonl`. Full
reference: `documentation/v2-storage.md`; benchmark and gate verdicts:
`docs/phase3-benchmark-v1-vs-v2.md`.

### RAM-first engine (Phase 1, slice 1C landed)

The v2 mmap backend is the baseline being replaced by a RAM-first engine
(authoritative `DocVal`/`DocRecord` in RAM, op-level WAL with group commit,
single visibility lock, bounded change ring, non-blocking compaction). Phase 0
froze the contracts and built the trust harness; **slice 1C adds the engine as a
third storage format** — v1 and v2 are untouched and still the default for
existing folders.

- `docs/contract_ram-first-engine.md` — normative: RAM state, durability and
  group commit, failure/poison rules, the recovery+compaction crash matrix,
  change-feed and memory governance.
- `docs/contract_http-api.md` — frozen `ndb serve` HTTP surface derived from
  `src/server.rs` (routes, shapes, status codes, reserved 503/507 codes).
- `src/io.rs` — `Io` trait (`RealIo`; `FaultIo` under `#[cfg(any(test, feature =
  "fault-io"))]`) covering every durable step, so the fault matrix can fail or
  kill the process at an exact operation.
- `src/ram_format.rs` — RAM-first format **v3** (`manifest.json`, streamed
  `base.gen_<N>.ndb`, op-level `wal.gen_<N>.ndb` with a trailing `Commit`
  frame): manifest/base writers and the WAL writer go through `Io` at every
  mutation; `scan_ram_wal` is read-only and separates a torn tail (`R0`) from
  mid-log corruption; `repair_ram_wal` / `fsck_ram_dir` for recovery.
- `src/ram_engine.rs` — the engine. `RwLock<RamState>` is the one visibility
  boundary (`docs`, `deleted`, indexes, lineage, manifest, change ring,
  pins); a per-DB FIFO queue with a single commit leader validates a batch
  against current RAM + earlier staged jobs, appends op-level frames
  (`Insert`/`Replace`/`Patch`/`Delete`/`Restore`) plus one `Commit` in a single
  `write_all` + `sync_all`, and only then publishes. Transient I/O
  (`Interrupted`, Windows 32) retries after truncating the batch back to its
  start offset; anything else poisons the DB (`Error::Poisoned`, HTTP 503) with
  nothing published. Index DDL is a schema-only batch that consumes **no**
  document seq (#17); `offset` is honoured without `limit` (#15); the fsync is
  unconditional (#16). Memory is governed by `RamConfig` (unpinned ring
  eviction first, then `Error::MemoryLimitExceeded` / HTTP 507 before the WAL is
  touched).
- **Compaction (slice 2A) is non-blocking.** Three phases, serialised by a
  dedicated `compaction_lock`, never holding the commit or state lock across
  base serialization: (1) *freeze & durable rotate* under `commit_lock` —
  capture `cut_seq` + the `Arc<DocRecord>` handles, create
  `wal.gen_<N+1>.ndb`, persist `pending_wal_gen` in the manifest **before** the
  writer swaps; (2) *serialize* `base.gen_<N+1>.ndb` with **no lock held**;
  (3) *publish* — atomic manifest swap, prune the ring below
  `min(cut_seq+1, min_pinned_seq)`, then sweep superseded generations. A stall
  leaves `pending_wal_gen` naming a WAL that holds post-freeze writes; recovery
  replays base + WAL + pending (deduped by seq), re-commits the pending records
  into the active generation and rewrites the manifest before sweeping. A retry
  after a stalled step *reuses* that pending generation instead of rotating
  again (a second rotation would orphan acknowledged writes).
  `RamConfig::auto_compact_wal_bytes` (`None` for embedded opens,
  `Some(32 MiB)` via `RamConfig::server_default`) starts a WAL-growth thread
  that joins on `Drop`; there is no timer-driven compaction.
- **Pinned snapshots (slice 2A).** `RamEngine::pin_snapshot(ttl)` freezes the
  document set as `Arc<Vec<Arc<DocRecord>>>` (never a deep clone, never under
  `commit_lock`) and reserves ring entries at/after its `cut_seq`;
  `get_pinned_page` / `release_pin` serve and release it. `GET
  /v2/dbs/{name}/snapshot` on a RAM database pages the pinned `Arc` set (only
  the requested page is converted to JSON) and the registry's reaper thread
  (2 s wake, exits on `request_process_shutdown`) releases expired pins for
  every mounted DB, so an idle server never holds one.
- `StorageFormat::{Auto,V1,V2,Ram}` — `ram`/`v3` parse; `Auto` routes a folder
  whose `manifest.json` is version 3 to the engine (v2 manifests carry
  `format_version`); `Database::open_ram`, `open_ram_with_config`,
  `open_ram_with_io` (test/`fault-io`), `open_with_ram_config` (serve),
  `is_ram`, `ram_stats`, `pin_snapshot` / `pinned_page` / `release_pin` /
  `reap_expired_pins`.
  `GET /stats` and `GET /v2/dbs/{name}/stats` add a `ram` block
  (live/index/ring bytes, oldest retained seq, expired cursors, pins, poison,
  WAL bytes, stage timings); `/v2/dbs` reports `"format": "ram"`.
- `tests/support/refdb.rs` — `RefDb`, a sequential reference model used as the
  differential oracle; `tests/support/crash_child.rs` — out-of-process crash
  runner (`current_exe()` + `NDB_CRASH_CHILD_ROLE`).
- Tests: `cargo test --features fault-io --test phase18_ram_engine_tests` (the
  10 000-op differential oracle, the three fix regressions, 4 MB structural
  sharing/WAL growth, 8-writer/4-reader group commit, poison injection, memory
  budget); `--test phase19_compaction_and_faults_tests` (every §4 crash row R0,
  R1, C1–C7 out-of-process through `crash_child`, mid-log corruption + repair,
  compaction under live readers/writers, pinned-snapshot retention and
  backpressure, WAL-growth auto-compaction, the RAM `/snapshot` pin route and
  the reaper); `--test refdb_oracle_tests --test phase17_fault_injection_tests`;
  `cargo test --features fault-io --lib io::tests`.
- Baseline: `cargo run --release --example corpus_baseline -- [--corpus
  memories|chat|all]` (default `all`) measures the Phase 0 workloads — warm
  `get`/mget/`$eq`/scan, durable `set`/`update`/`array_push` under
  `Persistence::Immediate`, and 4 readers under a writer — against **read-only
  copies** of `D:\DEV\mcp_server\data\memories` and
  `D:\SRV\LLM-Gateway-Chat\server\data\herrbasan` made in a `tempfile::tempdir()`.
  Frozen numbers, hardware and reproduction: `docs/phase0-baseline.md`.

## Query Layers

1. **Layer 1 — O(1) CRUD:** `insert`, `get`, `update`, `delete`, `array_push`. Direct HashMap operations.
2. **Layer 2 — Field Queries:** `find(field, value)`, `find_where()`, `find_range()`. Accelerated by opt-in hash and BTree indexes.
3. **Layer 3 — JSON AST Queries:** MongoDB-style filter objects with `$eq`, `$ne`, `$gt`, `$gte`, `$lt`, `$lte`, `$in`, `$nin`, `$exists`, `$and`, `$or`, `$not`, and dot-notation nested field access.

> Full-text search is **not** a query layer. `nVDB` owns it (BM25 + dense vectors + rerank); nDB answers structured queries only.

## File Buckets

Named binary storage with **SHA-256 content-hash deduplication**. Identical files are stored once, under the **full 64-char hash** (legacy 8-char ids stay readable); a dedup hit is verified (size + re-hash) so a damaged blob is reported, never returned. Files are referenced by compact `bucket:hash.ext` strings (nURI) or any of `/api/buckets/{b}/{f}`, `/file/{b}/{f}`, `/files/{b}/{f}`, `/v2/dbs/{db}/files/{b}/{f}` — all count as live references. Soft-delete moves files to `_trash/files/` rather than removing them. Garbage collection (`gc_buckets`) sweeps unreferenced files to trash and serializes with `store_file` on the writer lock. Targeted release (`release_file`) checks live **and restorable (trash) documents** before trashing a specific file.

## Atomic Delta Updates

For massive documents (e.g., 3 MB conversation objects), replacing the entire document on every change is catastrophic for I/O. nDB supports writing tiny delta operations to the append-only log:

```json
{"_id": "chat_123", "_op": "array_push", "field": "messages", "value": {...}}
```

On load, patches are replayed in memory. On compaction, patches are baked into a fresh base document.

## Sync Feed

`Database` exposes the journal as a replayable feed. One commit = one event, in journal order, assigned inside the writer lock after the append succeeds (a failed append moves neither `seq` nor the feed).

| Call | Contract |
|---|---|
| `lineage()` | `SourceLineage { source_epoch_uuid, last_seq, min_available_seq }` |
| `snapshot_at()` | `(lineage, docs)` under the writer lock; docs sorted by id |
| `changes_since(seq)` | `ChangeBatch` of events with `seq > request`, ascending; `CursorAhead` past `last_seq`, `CursorExpired` when compaction rewrote them away |

- `ChangeKind` is `Upsert(full post-mutation doc)` or `Delete` — the feed carries whole documents, never the patch line.
- A no-op (`set`/`remove`/`array_push` → `Ok(false)`) is not a commit: no line, no `seq`, no event.
- `compact()` and `export_snapshot()` preserve `source_epoch_uuid` and `last_seq`, set `min_available_seq = last_seq + 1`, and leave the rewritten docs as base state (no `_seq`).
- A file without `source_epoch_uuid` is pre-lineage: its epoch is derived deterministically from `_meta.created` (same file → same identity), and every base line replays as a commit.
- In-memory databases carry a full lineage and feed; only the disk append is skipped.

## HTTP Service (`ndb serve`)

Resident daemon (`src/server.rs`) — hand-rolled HTTP/1.1 on `std::net`, no async runtime. One process can now own **many databases**: `DbRegistry` maps name → `Arc<Database>`, with a canonical-path set preventing the same journal from being mounted twice, an optional `root_dir` for creation, and a service-wide persistence mode.

- Mount sources: positional `<path>` (legacy, mounted as `default`), repeatable `--db <name>=<path>`, `--dir <root>` (auto-mounts child folders holding `data.jsonl` or `meta.json`), and `--config <file.json>` (`bind/port/token/persistence/scheduled_ms/dir/max_limit/default_limit/dbs`; unknown fields rejected).
- Startup refuses: no source at all, an unopenable DB, a duplicate name or canonical path, an invalid DB name (`[a-zA-Z0-9._-]+`, not `.`/`..`), a missing `--dir`.
- `GET /health`, `GET|POST /v2/dbs`, and per-DB `/v2/dbs/{name}/{stats,lineage,doc,docs/mget,query,find,facets,index,compact,flush,changes,snapshot,files/...}`. `POST /v2/dbs` accepts only `{name, persistence?}` (never a path) and is `403 create_disabled` without `--dir`.
- Legacy single-DB routes (`/stats`, `/doc`, `/query`, `/file/...`) keep working against `default`; `/v2/` errors are `{ok:false, error, message}`.
- **Transport:** `TCP_NODELAY` on every accepted connection; one `write_all` per response frame (head+body for ≤64 KB), binary bodies move into the socket without a clone; `Content-Length` always exact and `Connection: keep-alive` / `close` always truthful.
- **Keep-alive:** one `BufReader` persists across requests on a socket; the loop ends on `Connection: close`, HTTP/1.0 without keep-alive, clean EOF, a 5 s idle timeout between requests (30 s for a live request), or a shutdown flag.
- **Worker pool:** `config.threads` workers (0 = logical cores, min 4) pull from a bounded queue (`max_connections` deep). A full queue answers `503` — bursts never spawn unbounded OS threads.
- **Graceful shutdown (M3):** Ctrl-C / SIGINT (Windows `SetConsoleCtrlHandler`, Unix `signal`) sets `server::request_process_shutdown()`. The accept loop stops, the queue closes, workers drain, and every mounted database is `flush()`ed before the process exits 0. A second Ctrl-C falls through to OS termination.
- Contract and route list: `docs/ndb-serve-design.md`.

## Node.js Access

**There is no Node binding.** The `napi/` crate was removed in **v2.0.0** (2026-11-23) along with its committed `.node` prebuilts, `index.js`/`vendor.js`/`setup.js` — zero NAPI anywhere in the ecosystem. Node and Electron consumers use `ndb serve` over HTTP; `mcp_server` and `LLM-Gateway-Chat` are live on it. `ndb serve` keeps one copy of the data in one Rust process instead of one copy per Node process, which is what the binding could never do.

**Full-text search is not part of nDB.** `src/search.rs`, `/search`, `POST /v2/dbs/{n}/search`, `type: "text"` in `/index` and the `text` constraint in `/query` were removed in v2.0.0 — `nVDB` owns all full-text, lexical, vector, hybrid and reranked search. Old `_index_*.fti` files left on disk are warned about and ignored at v2 open, so existing folders mount unchanged.

## Current Implementation Status

| Feature | State |
|---|---|
| Core CRUD + persistence | Complete |
| 3-layer query API | Complete |
| Hash + BTree indexes | Complete — typed `OrderedValue` keys (numeric order, dot paths, array element keys), index-only narrowing on `find`/`find_range` |
| Database-as-a-Folder | Complete |
| File buckets with SHA-256 dedup | Complete |
| `array_push` delta (Rust core) | Complete |
| Delta updates in Node.js backend | Complete (`arrayPush`, `set`, `remove` wired and used by consumers; ops return an `applied` flag — `Ok(false)`/`false` = no-op, writes nothing, D1 2026-09-28) |
| `release_file` + `gc_buckets` (Rust) | Complete |
| TTL-based trash purging | Complete (`with_trash_ttl` + background thread; purge serialized against delete's trash append via `trash_lock`, B3 2026-09-28) |
| Failure discipline | Complete — no bare `let _ =` on a `Result`; preconditions return `Err`, ancillary cleanup is reported (`src/report.rs`); a document is never reported deleted when its trash record could not be written |
| Journal-first write ordering | Complete — every mutating op appends before any in-memory/index/ref state moves; a failed append changes nothing (fault-injected tests) |
| Durability hardening (Phase 1b slice 1) | Complete — torn-tail truncation + mid-journal corruption refusal + `ndb repair`, per-DB OS writer lock, transactional `apply_ops` with per-doc `doc_seq`/`If-Match` (`409 conflict`), bucket ref normalization + GC/store serialization + full-SHA-256 dedup verification, durable doc-trash `sync_all`, parent-dir fsync after renames, real `Scheduled` flusher, Windows reserved/case-folded DB names, mget cap, pinned snapshot pagination; `tests/phase14_durability_hardening_tests.rs` |
| Hot-path performance (Phase 1b slice 2) | Complete — O(1) `insert` (no per-write id-set clone), real query planner (`$eq`/`$in` → hash/BTree, ranges merged into one BTree interval, smallest-first intersection), index-backed `find_range`, `Arc<Value>` store + change log (one document instance per write), O(1) `/stats`, `TCP_NODELAY` + single-frame responses + bounded worker pool + HTTP keep-alive + Ctrl-C drain/flush, `autobenches = false` (zero Cargo warnings); `tests/phase15_perf_and_planner_tests.rs` |
| Durable lineage + sync feed | Complete — `_meta` lineage (`source_epoch_uuid`/`last_seq`/`min_available_seq`), `_seq` on every committed line, `lineage()`/`snapshot_at()`/`changes_since()`, replay-on-open incl. legacy + compacted journals; `tests/phase0_sync_feed_tests.rs` |
| `meta.json` bucket policies | Complete — `kind`, `onDocumentDelete`, `ttl_seconds`, `reserved_ttl_seconds` enforced at open; malformed policy fails loudly; schema validation still unenforced |
| Item buckets (`kind: "items"`) | Complete — item folders + `items.jsonl` journal, reserve/commit/trash/restore, reserved sweep, `verify` integrity; CLI wired |
| Multi-DB HTTP service (`ndb serve`) | Complete (step 1) — `DbRegistry` mounts arbitrary DB paths by name, `--db`/`--dir`/`--config`, `/health` + `/v2/dbs` + per-DB CRUD/query/sync/bucket routes; `tests/phase13_multi_db_serve_tests.rs` |
| Schema validation from `meta.json` | Not implemented (`meta.json` schemas block ignored) |
| v2 binary/mmap storage backend (Phase 3, slice 3D) | Complete — `manifest.json` + mmap base generations + CRC32 WAL; zero-copy directory and persisted index sections (fixed-stride offset tables, O(1) slot access, O(log n) id lookup, `&[u32]` slot views); no per-document slot map (superseded bitset + WAL overlay, empty at cold boot); raw-JSON filter/sort-key scanning with bounded top-K and projected/raw materialization (`src/v2_json_scan.rs`); per-DB leader/follower group commit (one fsync per batch, RAM published only after fsync), bounded 1024-entry change ring with on-disk WAL fallback, non-blocking compaction with a mirror WAL and a few-KB freeze; `StorageFormat::{Auto,V1,V2}` gains `Database::open_v2*`; CLI `init --v2`, `migrate`, `fsck`, v2 `repair`, `export --jsonl`; `POST /v2/dbs` accepts `format: "v2"`. Reference: `documentation/v2-storage.md`; benchmark and gate verdicts: `docs/phase3-benchmark-v1-vs-v2.md`; tests: `tests/phase3_slice3b_v2_tests.rs`, `tests/phase16_v2_storage_tests.rs`, `tests/phase15_perf_and_planner_tests.rs` |
| nURI `link` type enforcement | Not implemented (file refs detected heuristically by string match, not by schema) |
| Bucket migration script for legacy data | Not bundled here (the consumer's `migrate-ndb-to-folder.js` was used in production) |

## Release Path (v2.0.0, standalone service)

**The git tag is the version truth.** Cutting a release:

1. Bump `Cargo.toml` and root `package.json` in one commit (there is one Cargo package now — the workspace and the `napi/` member are gone).
2. `cargo build --release --bin ndb`; copy `target/release/ndb.exe` to `bin/ndb.exe` (bin/ is gitignored — the CLI ships via the release, not the repo).
3. Oracles before tagging: full `cargo test` (all phases, unmodified) and `cargo check --all-targets` with zero warnings.
4. `git tag -a vX.Y.Z`, push, then `gh release create vX.Y.Z` with the `ndb.exe` artifact and a `.sha256` sidecar (`<hash>  <name>`, lowercase hex). Release matrix is **Windows x64 only** for now.

## What nDB Is Not

- Not a general-purpose network database. `ndb serve` is a single-process, in-memory HTTP daemon for a handful of trusted LAN clients — not a clustered or multi-tenant server.
- Not a relational database. No joins, no SQL.
- Not designed for terabyte-scale datasets. Everything lives in memory; persistence is for durability, not paging.
- Not a replacement for PostgreSQL or MongoDB in multi-user server deployments.

## Key Concepts for Agents

- **`_id`** — 16-character base62 NanoID, optionally prefixed (`conv_V1StGXR8Z5jdHi6B`). Generated client-side, collision-checked against the in-memory set.
- **`_seq`** — engine-owned commit sequence on a journal line. Stripped before the document is stored, so no read API returns it.
- **`SourceLineage`** — `source_epoch_uuid` + `last_seq` + `min_available_seq`: what a sync client resumes from and when it must resync.
- **Tombstone** — A JSONL line with `_deleted` timestamp. Marks a document as deleted without removing history.
- **Compaction** — Rewrites the JSONL file to contain only active documents, archiving tombstones to `_trash/docs/`.
- **FileRef** — The `bucket:hash.ext` compact string that links documents to binary files in buckets.
- **nURI** — The planned universal link format (`bucket:hash.ext` for internal, `https://...` for external). Not yet enforced by the schema validator.
