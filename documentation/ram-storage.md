# v3 RAM-first Storage

> The only binary engine since **v3.0.0** (2026-11-25). Normative contract:
> `docs/contract_ram-first-engine.md`. Format/engine code: `src/ram_format.rs`,
> `src/ram_engine.rs`, `src/crc32.rs`. The retired v2 mmap engine is described
> (historically) in `v2-storage.md`.

## Folder

```
<db>/
  manifest.json        sole generation + lineage authority (`version: 3`,
                       source_epoch_uuid, base_gen, wal_gen, pending_wal_gen,
                       last_seq, min_available_seq, indexes,
                       classification_profile, field_mapping)
  base.gen_<N>.ndb     immutable snapshot; every record CRC-32 framed
  wal.gen_<N>.ndb      op-level append log; each batch ends in a `Commit` frame
  .lock                OS writer lock (held for the handle's lifetime)
  meta.json            bucket policies (format-agnostic)
  _files/ _trash/      binaries and trash (format-agnostic)
```

A folder whose `manifest.json` is `format_version: 2` (the retired mmap format) is
**refused loudly** at open. Convert it with the preserved v2/transition binary
(`ndb migrate <path> --ram`) first.

## RAM is the store

- Authoritative documents are `DocVal`/`DocRecord` trees in RAM plus Hash/BTree
  indexes. **Nothing is mmap'ed**; no index section is read from disk.
- A cold open replays `base` then `wal` into RAM once, then every read is served
  from `RwLock<RamState>` (`docs`, `deleted`, indexes, lineage, manifest, change
  ring, pins).
- `RwLock<RamState>` is the single visibility boundary: one writer at a time,
  many concurrent readers.

## Writes: op-level WAL + group commit

Every mutation queues a job on a per-DB FIFO queue. The first free thread becomes
leader and, inside one batch:

1. validates each job against current RAM + earlier staged jobs in the batch;
2. appends op-level frames (`Insert`/`Replace`/`Patch`/`Delete`/`Restore`) plus
   **one** `Commit` frame in a single `write_all` + `sync_all`;
3. publishes to RAM only after the fsync succeeded.

- A failed fsync publishes **nothing** and returns the error to every waiter.
- Transient I/O (`Interrupted`, Windows error 32) truncates the batch back to its
  start offset and retries.
- A non-transient failure **poisons** the DB: `Error::Poisoned` / HTTP
  `503 db_poisoned`. Nothing after the failure is published.
- Index DDL is a schema-only batch that consumes **no** document seq.
- Recovery: a torn WAL **tail** is cleared at open with a warning; anything at or
  before a `Commit` frame is a loud failure (`ndb repair` quarantines and
  truncates the generation).

## Compaction (non-blocking)

Serialised by a dedicated `compaction_lock`; the commit/state lock is never held
across base serialization:

1. **freeze & durable rotate** (under `commit_lock`): capture `cut_seq` + the
   `Arc<DocRecord>` handles, create `wal.gen_<N+1>.ndb`, persist
   **`pending_wal_gen`** in the manifest *before* the writer swaps;
2. **serialize** `base.gen_<N+1>.ndb` with **no lock held**;
3. **publish**: atomic manifest swap, prune the ring below
   `min(cut_seq+1, min_pinned_seq)`, sweep superseded generations.

A stall leaves `pending_wal_gen` naming a WAL that holds post-freeze writes.
Recovery replays base + WAL + pending (deduped by seq), re-commits the pending
records into the active generation and rewrites the manifest before sweeping. A
retry after a stalled step **reuses** the pending generation instead of rotating
again (a second rotation would orphan acknowledged writes).

`RamConfig::auto_compact_wal_bytes` (`None` for embedded opens, `Some(32 MiB)`
via `RamConfig::server_default`) starts a WAL-growth thread; there is no
timer-driven compaction.

## Memory governance

`RamConfig::max_memory_bytes` bounds live documents + index + retained ring
bytes (`None` = unbounded): unpinned change-ring entries are evicted first; if
the budget is still exhausted the write fails with
`Error::MemoryLimitExceeded` / HTTP **`507 memory_limit_exceeded`** **before** the
WAL is touched. Change retention is bounded by `change_ring_cap_events` (1024)
and `change_ring_cap_bytes` (64 MiB). `GET /stats` and
`GET /v2/dbs/{name}/stats` report a `ram` block (live/index/ring bytes, oldest
retained seq, expired cursors, pins, poison, WAL bytes, stage timings).

## Change feed

A bounded in-RAM ring (1024 events). `changes_since(seq)` answers from RAM while
the cursor is inside the ring, otherwise it streams the WAL. Same
`CursorAhead` / `CursorExpired` contract and `source_epoch_uuid` as v1.

## Pinned snapshots

`pin_snapshot(ttl)` freezes the document set as `Arc<Vec<Arc<DocRecord>>>` (no
deep clone, never under `commit_lock`) and reserves ring entries at/after its
`cut_seq`; `get_pinned_page` / `release_pin` serve and release it. `GET
/v2/dbs/{name}/snapshot` pages the pinned set, releases the pin on the completing
page, and a reaper thread (2 s wake) expires stale pins.

## HTTP status codes

| Code | Error | Cause |
|---|---|---|
| `503` | `db_poisoned` | non-transient I/O failure; DB unusable until repaired |
| `507` | `memory_limit_exceeded` | `max_memory_bytes` exhausted before the WAL write |
| `422` | `scan_budget_exceeded` | query exceeded `max_scan_docs` (`scanned`, `max_scan`) |
| `409` | `cursor_expired` / `cursor_ahead` / `conflict` | change cursor out of range; `expected_seq`/`If-Match` mismatch |

## CLI

`ndb init <path>` creates v3 by default (`--ram`/`--v3` redundant, `--v2` refused);
`ndb migrate <path>` converts v1 → v3 (staged in `.migrating_ram/`, doc-by-doc
parity before and after publishing, `data.jsonl.v1.bak` preserved);
`ndb fsck` / `ndb repair` / `ndb export --jsonl` / `ndb compact` /
`ndb info` auto-detect the format. See `cli.md`.
