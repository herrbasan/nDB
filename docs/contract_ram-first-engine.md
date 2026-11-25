---
title: Contract — nDB RAM-First Engine
status: frozen for Phase 1 implementation
date: 2026-11-25
scope: docs/contract_ram-first-engine.md
supersedes: the durability/visibility sections of documentation/v2-storage.md for the RAM-first engine
---

# Contract — nDB RAM-First Engine

This document is the normative contract the RAM-first engine must satisfy. It
is written against the settled dev plan
(`D:\MCP_Storage\documentation\Workshop\dev-plan_ndb-ram-first.md`) and the
Phase 0 audit of the current v2 backend. Where this document and the current
code disagree, **this document wins and the code is the bug** — except where a
row is explicitly marked *current behaviour*.

Vocabulary: **acknowledged** = the call returned `Ok`; **unacknowledged** =
the call returned `Err` or never returned; **published** = visible to a reader
through the RAM state.

---

## 1. Authoritative RAM state

### 1.1 Value tree

```rust
enum DocVal {
    Null,
    Bool(bool),
    Number(serde_json::Number),
    String(Arc<str>),
    Array(Arc<Vec<Arc<DocVal>>>),
    Object(Arc<BTreeMap<Box<str>, Arc<DocVal>>>),
}

struct DocRecord {
    id: Arc<str>,
    doc_seq: u64,      // seq that produced this record
    doc: Arc<DocVal>,
    approx_bytes: usize,
}
```

Rules:

1. **`DocVal` is internal.** Every public Rust API keeps `serde_json::Value` at
   the boundary. `DocVal` serialization must be byte-identical to `serde_json`
   output for the same value (key order, number formatting, `OrderedF64`
   semantics unchanged). The indexed-vs-scan oracle depends on this.
2. **Structural sharing is mandatory, not an optimisation.** A patch copies
   only the nodes on the changed path. Unchanged subtrees are shared by `Arc`
   refcount — appending to a 4 MB conversation must copy kilobytes.
3. **`approx_bytes` is maintained by delta** (parent − replaced child + new
   child, including node/`Arc` overhead at ~1.3×). It is never computed by
   walking a subtree: a walk would double-count shared subtrees that the change
   ring holds.
4. **Immutability.** A published `DocRecord` is never mutated. A new version is
   a new `Arc<DocRecord>`; the old one may live on in the change ring or a
   pinned snapshot.

### 1.2 One visibility boundary

All of the following live behind **one** reader/writer lock and are replaced
together, in one critical section:

- `docs: HashMap<Arc<str>, Arc<DocRecord>>`
- every secondary index (hash and ordered)
- `lineage.last_seq` (and `min_available_seq`)
- the change ring

Guarantees:

1. **Sequence-consistent reads.** A single read (`get`, `mget`, `query`,
   `facets`, range read, snapshot) observes exactly one published sequence. It
   never sees a document without the index entry that matches it, or a change
   ring that disagrees with the document set.
2. **Read-your-writes.** An acknowledged write is visible to the next read on
   the same database, with no waiting.
3. **Index completeness.** For every indexed field, the index is a *superset*
   of what a linear scan matches (arrays contribute both the whole value and
   each element), so the planner can narrow but never change an answer.
4. **Blocking.** Readers take the read lock and never wait on disk I/O. Writers
   hold the write lock only to publish — never across an fsync.

---

## 2. Durability & group commit

### 2.1 Write path

1. A caller enqueues its mutation and waits on a per-request completion
   channel. Nothing is visible yet.
2. A single **commit leader** drains the FIFO queue and, under the writer
   ordering lock, validates each job (`if_seq`, patch path resolution, document
   existence) against the current published state. Validation failures are
   returned to their caller and stage nothing; the rest of the batch continues.
3. The leader encodes the surviving operations into **one** buffer:
   operation frames (`Insert`/`Replace`/`Patch`/`Delete`/`Restore`, each
   length-prefixed and CRC-32 framed with its `seq`) followed by
   `Commit { start_seq, end_seq, batch_crc }`.
4. **One `write_all` + one `sync_all`.** That fsync is the durability point.
5. Only after `sync_all` succeeds does the leader take the write lock and
   publish: documents, indexes, `lineage.last_seq`, change-ring entries.
6. Callers are released **after** publication.

### 2.2 Acknowledgement rule

- A mutation returns `Ok` **only after** its batch is durably synced.
- **Acknowledged writes are never lost** on process crash or power failure.
- **Unacknowledged writes may or may not appear** after recovery: if the fsync
  completed but the process died before replying, recovery keeps the batch. A
  caller that received an error must not assume the write is absent; it must
  re-read. This asymmetry is deliberate and is a caller-visible contract.
- **A no-op is not a commit.** `set`/`remove`/`array_push` returning `Ok(false)`
  writes nothing, consumes no `seq`, and emits no change event.

*(Current v2 behaviour, herrbasan/nDB#16: the group commit fsyncs only under
`Persistence::Immediate`; the default `Lazy` acknowledges before the fsync.
Phase 1 makes the fsync unconditional — this section describes that target, not
the v2 default.)*

### 2.3 Batch invariants

- One batch = one fsync. `seq` is contiguous inside the batch.
- `Commit.end_seq - Commit.start_seq + 1` equals the number of operation frames
  in the batch; `batch_crc` covers the frames it names.
- A batch never mixes operations from different databases (one writer queue per
  DB).
- Failure of the batch's write/fsync fails **every** caller whose job staged
  successfully in it. Partial success within one fsync is impossible.

---

## 3. Failure & poison rules

Three failure classes, three distinct responses. Collapsing any two of them is
a contract violation.

| Class | Example | Required response |
|---|---|---|
| **Transient I/O** | `ERROR_SHARING_VIOLATION`, `Interrupted` | bounded retry (small, fixed attempts) inside the writer; if it still fails, it becomes *uncertain*. |
| **Uncertain durable tail** | `write_all` or `sync_all` failed, or returned a short/failed flush | **poison the database.** Nothing is published. Writes are refused with `Error::Poisoned` until the database is reopened/recovered. Reads continue from RAM. |
| **Corruption** | base/WAL frame fails CRC or structure before the last commit | `Error::Corruption` naming file + seq/offset. No automatic repair, no silent truncation of committed data. |

### 3.1 Poisoning

- Trigger: any error that leaves the durable tail's state unknown.
- Effect: the DB is marked poisoned in memory. Every subsequent mutation fails
  immediately with `Error::Poisoned` — no retry, no queueing, no fallback to
  "just wrote to RAM". Reads are unaffected (RAM is authoritative and correct
  up to `last_seq`).
- Clearing: only reopen/recovery. There is no API to un-poison a running
  process. (A caller that never fsynced and knows the OS had no I/O error still
  cannot clear it — the DB cannot prove that.)
- HTTP surface: `503`, `error: "db_poisoned"`.

### 3.2 Post-fsync publication failure

If the fsync succeeded but publishing into RAM fails (index update, allocation,
invariant check), the process **must abort loudly** — no rollback, no partial
publish, no "recover in place". The durable log already contains the batch, so
a clean restart replays it and reaches the correct state. Continuing with RAM
that disagrees with the log is the only unrecoverable outcome and is therefore
forbidden.

This is an `abort`/`process::exit`, not a `Result`: a partially published state
has no meaningful error value to return to individual callers.

### 3.3 What must never happen

- A read lock held across an fsync.
- RAM published before the fsync (breaks "acknowledged ⇒ durable" and
  "unacknowledged ⇒ may appear"). A crash after such a publish would *lose* an
  acknowledged write.
- A silent fallback from "durable" to "best effort".
- Swallowing an error from `Io` at any step.

---

## 4. Recovery & compaction crash matrix

Recovery invariants (apply to every row):

- **Acknowledged writes survive.** Any batch whose `Commit` frame is intact in
  a WAL generation that startup is allowed to read is replayed.
- **Only a torn tail is discarded.** Trailing bytes after the last intact
  `Commit` are truncated (with a warning) and fsynced **before** any new append
  can occur. Corruption before the last intact `Commit` is loud, never repaired
  implicitly.
- **`seq` never resets** — not on restart, not on compaction.
- **`pending_wal_gen` is never swept.** Startup deletes obsolete generations
  only when they are neither `base_gen`/`wal_gen` nor `pending_wal_gen`.

Manifest fields: `base_gen`, `wal_gen`, `pending_wal_gen` (absent = none),
`last_seq`, `min_available_seq`, `source_epoch_uuid`, index specs.

| # | Crash point | Durable state on disk | Required recovery outcome |
|---|---|---|---|
| R0 | mid-batch, before `Commit` frame is fully written | WAL holds N operation frames with no terminating `Commit` | Last intact `Commit` wins. The incomplete frames are a torn tail: truncate at the last good offset, fsync, warn. All of *that* batch's writes are absent; earlier acknowledged batches are intact. |
| R1 | after `sync_all` of a batch, before publish | WAL holds a complete batch + `Commit` | Replay includes the batch. Those writes may appear even though no caller was acked (see §2.2). |
| C1 | **before `pending_wal_gen` is written to the manifest** | Old manifest (`base.gen_N` + `wal.gen_N`) is complete; no write has reached `wal.gen_<N+1>` | Load `base.gen_N` + `wal.gen_N`. `wal.gen_<N+1>` does not exist (or is an unreferenced empty stub) and is removed as an orphan. Compaction did not happen. |
| C2 | **after `pending_wal_gen = N+1` is written, before/during the base write** | New manifest references `pending_wal_gen = N+1`; `base.gen_<N+1>` is absent or partially written; `wal.gen_<N+1>` holds writes that landed after the freeze | Load `base.gen_N` + `wal.gen_N` + `wal.gen_<N+1>` applied in sequence order (dedupe by `seq`). `base.gen_<N+1>` is discarded and its file removed. `pending_wal_gen` is cleared by the recovery rewrite of the manifest. No acknowledged write is lost; no duplicate `seq` is applied twice. |
| C3 | **during the base write** (`base.gen_<N+1>` partially serialized) | Same as C2 plus a truncated `base.gen_<N+1>` with a valid-looking header | Same as C2. A base file is usable only if its header, directory and all CRCs verify; anything else is an incomplete compaction artefact. Never "repair" a base by scanning it. |
| C4 | **before the publish swap** (new base and WAL are fully durable, manifest still names N) | `base.gen_<N+1>` and `wal.gen_<N+1>` complete; manifest still `base_gen=N, wal_gen=N, pending_wal_gen=N+1` | Same as C2: replay `base.gen_N` + `wal.gen_N` + `wal.gen_<N+1>`. The complete `base.gen_<N+1>` is discarded (recompaction is cheap); correctness does not depend on adopting it. |
| C5 | **after the publish swap** (manifest atomically replaced) | Manifest: `base_gen=N+1, wal_gen=N+1`, no `pending_wal_gen`; `base.gen_<N+1>` durable | Load `base.gen_<N+1>` + `wal.gen_<N+1>`. Old `base.gen_N` / `wal.gen_N` are orphans and are removed. `last_seq` preserved; `min_available_seq` already advanced to the freeze point + 1 by the swap. |
| C6 | **during recovery tail-truncation** | WAL truncated in the page cache, or partially; the fsync of the truncation may not have happened | Recovery is idempotent: rescan from the header, find the last intact `Commit`, truncate again to the same offset, fsync. A crash during truncation leaves either the pre-truncation file (≥ the same good offset) or a shorter file whose good prefix is unchanged. Never append before a successful truncate + fsync. |
| C7 | during recovery replay itself | Process dies while rebuilding RAM | Disk is unchanged; restart reruns recovery from the same durable state. Replay must therefore be pure (no on-disk side effects except the tail truncation of R0/C6). |

Manifest writes are always: temp file → `sync_all` → atomic rename → parent
directory fsync (§5). A crash inside that sequence leaves either the old or the
new manifest, never a mixture.

---

## 5. Change feed & memory governance

### 5.1 Change ring

- Entries: `{ seq, epoch, kind: Upsert | Delete, id, post_image: Option<Arc<DocRecord>> }`.
  `post_image` is the exact published record for that `seq` — never a later
  lookup, never a re-parse.
- Bounded by **both** retained bytes and event count (`change_ring` bytes and
  `CHANGE_RING_CAP`). Count alone is not a bound: ten 4 MB chat post-images are
  40 MB regardless of event count.
- Eviction is oldest-first, and **unpinned entries evict before any pinned
  entry**. An evicted `seq` moves the feed's floor: `min_available_seq` becomes
  the oldest retained `seq`, and `changes_since` below it fails with
  `409 cursor_expired`.
- `changes_since(since)`:
  - `since > last_seq` → `409 cursor_ahead`
  - `since == last_seq` → empty batch
  - `since + 1 < min_available_seq` → `409 cursor_expired`
  - otherwise ascending events with `seq > since`; the ring serves what it
    holds, older retained history is rebuilt from the WAL.
- Compaction sets `min_available_seq = last_seq + 1` and clears the ring: the
  rewritten documents are base state, not events.
- **Index DDL is not a document event.** `create_index`/`drop_index` may consume
  a `seq` or not, but the choice must be identical on every backend and must be
  documented, because a consumer's watermark cannot advance past a seq that
  never appears in the feed. (Current v2 burns a seq with no event; v1 burns
  none — herrbasan/nDB#17. Phase 1 must settle it.)

### 5.2 Pinned snapshots

- `/snapshot` pins a frozen `Vec<Arc<DocRecord>>` plus its `cut_seq`. The pin
  reserves the ring entries at and after `cut_seq`: they cannot be evicted
  while the pin lives.
- Pins expire after a TTL (default 300 s). Reaping runs on a **timer**, not on
  access, so an idle server always releases expired pins. An expired or unknown
  `snapshot_id` answers `410 snapshot_expired`.
- The HTTP page route releases a pin early once it serves the page that
  completes the read (`offset + count >= total`): a walk that finished — one
  page or several — must not keep ring history alive for the rest of the TTL.
  An unfinished walk stays pinned.
- A pin never deep-clones the document set: it holds `Arc`s. Pinning is
  therefore cheap even for hundreds of MB.

### 5.3 Memory accounting and the hard limit

Tracked per DB and reported in `/stats`: live document bytes, index bytes,
retained old-root bytes (change ring + active pins), staged/queued bytes,
`oldest_retained_seq`, consumer lag, expired-cursor count, active pin count.

Order of response under pressure:

1. Evict unpinned change-ring entries (advance `min_available_seq`; laggards get
   `cursor_expired`).
2. Reap expired pins (timer does this anyway).
3. **Refuse the write** with `507 memory_limit_exceeded`. Never evict a live
   document, never silently drop a pinned snapshot, never let the OS OOM-kill
   the process.

The limit is enforced against the *post-commit* estimate, not after allocation:
a write that would cross `max_memory_bytes` is rejected before it is staged.
`507 memory_limit_exceeded` is a property of the database's own budget; a
service-wide budget breach also answers `507` (never a silent restart).

*(Current v2 behaviour: no memory limit exists yet. This section is the
contract Phase 1 must implement; `/stats` gains the counters in the same
phase.)*

---

## 6. Non-goals (rejected by design)

- No second disk/mmap read engine, no per-document hot/cold tiering.
- No lossy write-behind / immediate-ack mode.
- No cross-document transactions. One document, or one atomic `apply_ops`
  batch on one document.
- No silent schema/preset fallbacks: an unknown operator, path, or index type
  is an error.
- No "repair" that invents data. Corruption is reported with its location;
  `ndb repair` quarantines a damaged suffix and reports the lost seq range.
