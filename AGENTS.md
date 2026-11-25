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

nDB is an **embeddable in-memory document database** built in Rust. It stores JSON documents in a `HashMap` for O(1) lookups and persists them to an append-only JSON Lines file (v1) or to the **v3 RAM-first** binary format (`manifest.json` + `base.gen_<N>.ndb` + op-level `wal.gen_<N>.ndb`). Since **v2.0.0** it is also the **standalone multi-DB HTTP service `ndb serve`** — the only non-Rust interface. The napi Node binding crate was removed in v2.0.0 (no `.node` prebuilts, zero NAPI); `mcp_server` and `LLM-Gateway-Chat` talk to `ndb serve`. Full-text, lexical, vector and hybrid search belong to `nVDB`, not here.

## Core Design

- **Single-writer, multi-reader** concurrency. One thread writes at a time; many threads can read simultaneously.
- **Append-only JSON Lines** persistence. Every write appends a line — payload + `\n` in one buffered `write_all`. On load the journal is streamed line by line; last write wins, tombstones mark deletions, delta patches are applied in order. A torn *final* line is truncated at the last good byte offset (with a warning); **mid-journal corruption fails loudly** (`Error::Corruption` naming the line) and is recovered with `ndb repair <path>`, which quarantines the file to `<file>.corrupt.<ts>.bak` and truncates at the last good line.
- **Durable lineage + change feed.** `_meta` carries `source_epoch_uuid`, `last_seq`, `min_available_seq`; every committed line carries its `_seq`. `lineage()`, `snapshot_at()` and `changes_since(seq)` expose the journal as a replayable feed under the writer lock. `_seq` is engine-owned and stripped before any document reaches memory, so no read API returns it. `seq` never resets — not on compaction, not on restart.
- **Database-as-a-Folder.** One directory per database: `data.jsonl` (documents — the file passed to `open()`), `_files/` (binary storage, created implicitly), `_trash/` (soft-deleted items), and `meta.json` (bucket declarations/policies — written by the CLI/migration, **read by the core at open**: `kind`, `onDocumentDelete`, `ttl_seconds`, `reserved_ttl_seconds`; the `schemas` block is still ignored).
- **Zero dependencies beyond the Rust standard library plus `serde`, `serde_json`, `parking_lot`, `fastrand`, and `thiserror`.** No external crypto, no external DB engines. SHA-256 for file deduplication is hand-rolled.

## Storage Backends (v1 JSONL / v3 RAM-first)

`Database::open` detects the backend from the folder: a `manifest.json` whose
`version` is 3 opens on the **RAM-first** engine, `data.jsonl` opens on **v1**,
and an empty folder initialises as v1. The public API is identical on both.
A `manifest.json` that names the retired **v2 mmap** format (`format_version`)
is refused loudly — there is no v3 downgrade path and no silent acceptance.

- **v3 files**: `manifest.json` (sole generation/lineage authority, `version:
  3`), `base.gen_<N>.ndb` (streamed immutable snapshot, every record CRC-32
  framed), `wal.gen_<N>.ndb` (op-level append log whose batches end in a
  `Commit` frame). Every parser validates CRC and structure; a torn WAL tail is
  cleared at open with a warning, anything at or before a `Commit` fails loud.
- **In-memory**: RAM is the store — `DocVal`/`DocRecord` trees plus Hash/BTree
  indexes; nothing is mmap'ed and no index section is read from disk. A cold
  open replays base + WAL into RAM once, then serves every read from `RwLock`
  state.
- **Queries**: filters and sort keys read the `DocVal` tree directly; the HTTP
  raw path serializes out of RAM (`query_raw`) and a field projection writes
  straight out of the tree (`project_write_json`).
- **Writes**: per-database group commit coalesces concurrent mutations into one
  `write_all` + `sync_all` of op frames plus one `Commit`; RAM is published only
  after the fsync, and a failed fsync publishes nothing.
- **Feed**: a bounded change ring (1024 events); older cursors are answered from
  the WAL.
- **Compaction** is non-blocking: freeze under a short lock (manifest + `Arc`
  handles), serialize the new base with no lock held, then publish with an
  atomic manifest swap.

Index declarations live in `ram_format::{IndexSpec, IndexKind}`
(`{"field": ..., "kind": "hash"|"btree"}` — the shape the live
`manifest.json` files already carry) and are rebuilt into RAM at open.
Checksums: `src/crc32.rs`. CLI: `ndb init [--ram]`, `migrate [--ram]`, `fsck`,
`repair`, `export --jsonl`. Reference: `documentation/cli.md`.

**v2 mmap was retired (2026-11-25).** `src/v2_engine.rs`, `src/v2_format.rs`,
`src/v2_json_scan.rs` and `src/mmap.rs` are gone; the code stays in git history
(`9118718` and earlier). A v2 folder must be converted by the preserved
v2/transition binary (`ndb migrate <path> --ram`) before this build can open it.
`StorageFormat::parse("v2")` and `ndb init --v2` both fail with that
instruction, and `Database::open` refuses a v2 manifest by name.

### RAM-first engine (the only binary backend)

The RAM-first engine is the shipped binary backend (authoritative
`DocVal`/`DocRecord` in RAM, op-level WAL with group commit, single visibility
lock, bounded change ring, non-blocking compaction). The v2 mmap engine it
replaced is retired and preserved only in git history.

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
  the requested page is converted to JSON), releases the pin as soon as it
  serves the completing page (`offset + count >= total`) and the registry's
  reaper thread (2 s wake, exits on `request_process_shutdown`) releases expired
  pins for every mounted DB, so an idle server never holds one.
- **File references on the RAM engine (#18).** The v3 write path commits inside
  the engine, so `Database::file_refs` cannot be counted there: a RAM insert
  extracts and counts the document's nURIs itself, and
  `update`/`set`/`remove`/`array_push`/`apply_ops` read the document around the
  delegated call and run the same before/after delta the v1 path uses.
  `ensure_ram_file_refs` reconciles the counters with the live set once (a v3
  folder opened on existing data starts empty), so `DELETE /doc/:id` moves the
  files only that document held into the bucket trash in the same call, and
  `release_file` / `DELETE /files/…` sees live and restorable references
  identically on every backend.
- `StorageFormat::{Auto,V1,Ram}` — `ram`/`v3` parse, `v2` fails with the
  retirement notice; `Auto` routes a folder whose `manifest.json` is version 3
  to the engine and a v2 manifest to a loud error (v1 folders hold
  `data.jsonl`); `Database::open_ram`, `open_ram_with_config`,
  `open_ram_with_io` (test/`fault-io`), `open_with_ram_config` (serve),
  `is_ram`, `ram_stats`, `pin_snapshot` / `pinned_page` / `release_pin` /
  `reap_expired_pins`.
  `ndb serve --format auto|v1|ram` sets the create format (**`ram` is the
  default** when the flag and the config file are silent) and also decides the
  format of a `--db name=<path>` mount whose path does not exist yet; an
  existing folder is always opened by detection, never by the flag.
  `GET /stats` and `GET /v2/dbs/{name}/stats` add a `ram` block
  (live/index/ring bytes, oldest retained seq, expired cursors, pins, poison,
  WAL bytes, stage timings); `/v2/dbs` reports `"format": "ram"`.
- **CLI.** `ndb init <path>` initialises **v3 RAM-first by default** (`--ram` /
  `--v3` accepted and redundant; `--v2` is refused with the retirement notice);
  `ndb migrate <path>` converts a **v1** journal to v3 — staged in
  `.migrating_ram/`, verified document-by-document (count, ids, JSON values,
  lineage, indexes) before *and* after publishing, with `data.jsonl.v1.bak`
  preserving the source; the manifest is written with
  `min_available_seq = last_seq + 1` because every document is folded into
  `base.gen_1.ndb` and no event history carries over. A folder already on **v2**
  is refused: the conversion must run on the preserved v2/transition binary.
  `ndb fsck` auto-detects the folder by manifest version and runs
  `fsck_ram_dir` + bucket checks; `ndb repair` quarantines and truncates each
  WAL generation (including `pending_wal_gen`) under the maintenance lock and
  prints the surviving seq; `ndb export --jsonl` is id-sorted with a `_meta`
  lineage line.
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

Named binary storage with **SHA-256 content-hash deduplication**. Identical files are stored once, under the **full 64-char hash** (legacy 8-char ids stay readable); a dedup hit is verified (size + re-hash) so a damaged blob is reported, never returned. Files are referenced by compact `bucket:hash.ext` strings (nURI) or any of `/api/buckets/{b}/{f}`, `/file/{b}/{f}`, `/files/{b}/{f}`, `/v2/dbs/{db}/files/{b}/{f}` — all count as live references, and `ndb verify` resolves the same set through `Database::normalize_file_ref` (a checker narrower than the collector reports clean while looking at nothing). Soft-delete moves files to `_trash/files/` rather than removing them; trashed entries are **never aged by `mtime`** (a rename preserves it) — a `<name>.trashed_at` sidecar records residence, `restore` removes it, and a missing/unreadable sidecar means retain. Garbage collection (`gc_buckets`) sweeps unreferenced files to trash and serializes with `store_file` on the writer lock; buckets declared `kind: "items"` are skipped entirely (their folders are keyed by item id and `items.jsonl` is engine-owned). Targeted release (`release_file`) checks live **and restorable (trash) documents** before trashing a specific file.

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

**Full-text search is not part of nDB.** `src/search.rs`, `/search`, `POST /v2/dbs/{n}/search`, `type: "text"` in `/index` and the `text` constraint in `/query` were removed in v2.0.0 — `nVDB` owns all full-text, lexical, vector, hybrid and reranked search. Old `_index_*.fti` files left on disk are warned about and ignored at open, so existing folders mount unchanged.

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
| TTL-based trash purging | Complete (`with_trash_ttl` + background thread; purge serialized against delete's trash append via `trash_lock`, B3 2026-09-28; residence comes from the `.trashed_at` sidecar written at delete/trash time, never from `mtime` — a missing or unreadable sidecar retains the entry, #14) |
| Failure discipline | Complete — no bare `let _ =` on a `Result`; preconditions return `Err`, ancillary cleanup is reported (`src/report.rs`); a document is never reported deleted when its trash record could not be written |
| Journal-first write ordering | Complete — every mutating op appends before any in-memory/index/ref state moves; a failed append changes nothing (fault-injected tests) |
| Durability hardening (Phase 1b slice 1) | Complete — torn-tail truncation + mid-journal corruption refusal + `ndb repair`, per-DB OS writer lock, transactional `apply_ops` with per-doc `doc_seq`/`If-Match` (`409 conflict`), bucket ref normalization + GC/store serialization + full-SHA-256 dedup verification, durable doc-trash `sync_all`, parent-dir fsync after renames, real `Scheduled` flusher, Windows reserved/case-folded DB names, mget cap, pinned snapshot pagination; `tests/phase14_durability_hardening_tests.rs` |
| Hot-path performance (Phase 1b slice 2) | Complete — O(1) `insert` (no per-write id-set clone), real query planner (`$eq`/`$in` → hash/BTree, ranges merged into one BTree interval, smallest-first intersection), index-backed `find_range`, `Arc<Value>` store + change log (one document instance per write), O(1) `/stats`, `TCP_NODELAY` + single-frame responses + bounded worker pool + HTTP keep-alive + Ctrl-C drain/flush, `autobenches = false` (zero Cargo warnings); `tests/phase15_perf_and_planner_tests.rs` |
| Durable lineage + sync feed | Complete — `_meta` lineage (`source_epoch_uuid`/`last_seq`/`min_available_seq`), `_seq` on every committed line, `lineage()`/`snapshot_at()`/`changes_since()`, replay-on-open incl. legacy + compacted journals; `tests/phase0_sync_feed_tests.rs` |
| `meta.json` bucket policies | Complete — `kind`, `onDocumentDelete`, `ttl_seconds`, `reserved_ttl_seconds` enforced at open; malformed policy fails loudly; schema validation still unenforced |
| Item buckets (`kind: "items"`) | Complete — item folders + `items.jsonl` journal, reserve/commit/trash/restore, reserved sweep, `verify` integrity; CLI wired |
| Multi-DB HTTP service (`ndb serve`) | Complete (step 1) — `DbRegistry` mounts arbitrary DB paths by name, `--db`/`--dir`/`--config`, `/health` + `/v2/dbs` + per-DB CRUD/query/sync/bucket routes; `tests/phase13_multi_db_serve_tests.rs` |
| Schema validation from `meta.json` | Not implemented (`meta.json` schemas block ignored) |
| v3 RAM-first binary backend | Complete (the only binary engine) — authoritative `DocVal` in RAM, op-level WAL with group commit, non-blocking compaction with `pending_wal_gen`, pinned snapshots, `FaultIo` crash matrix; CLI `init` (v3 by default), `migrate` from v1 (staged, doc-by-doc parity before and after publishing, `data.jsonl.v1.bak` preservation), v3 `fsck`/`repair`/`export --jsonl`; `tests/phase18_ram_engine_tests.rs`, `tests/phase19_compaction_and_faults_tests.rs`, `tests/phase20_cli_and_maintenance_tests.rs`, `tests/phase21_ram_http_contract_tests.rs` |
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
