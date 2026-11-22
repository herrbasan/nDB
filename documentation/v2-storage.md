# v2 Binary / mmap Storage

nDB has two storage backends behind one `Database` API. The format is detected
at open; nothing else about the API changes.

| Folder contents | Backend |
|---|---|
| `manifest.json` | **v2** — `base.gen_<N>.ndb` + `wal.gen_<N>.ndb` |
| `data.jsonl` | **v1** — JSON Lines journal |
| empty folder (or `data.jsonl` named explicitly) | v1, unless `StorageFormat::V2` is requested |

```rust
// Auto-detection (default)
let db = Database::open(dir)?;

// New v2 folder
let db = Database::open_v2(dir)?;
let db = Database::open_v2_with_options(dir, OpenOptions {
    format: StorageFormat::V2,
    persistence: Persistence::Immediate,
})?;
```

`StorageFormat::{Auto, V1, V2}` lives on `OpenOptions`. A folder holding
`manifest.json` opens on v2 even when the caller names `data.jsonl`; forcing
`V1` on a v2 folder is a corruption error, not a silent downgrade.

## Layout

```
<db>/
  manifest.json          generation + lineage authority (never derived from file names)
  base.gen_<N>.ndb       immutable, mmap'ed, CRC-32 protected snapshot
  wal.gen_<N>.ndb        append-only, CRC-32 framed WAL
  .lock                  OS writer lock (held for the handle's lifetime)
  meta.json              bucket policies (format-agnostic)
  _files/ _trash/        binaries and trash (format-agnostic)
```

`manifest.json` carries `format_version`, `base_gen`, `wal_gen`,
`source_epoch_uuid`, `base_seq`, `min_available_seq`, `doc_count`, `indexes`.
`fsck_v2_dir` cross-checks all of it against the files.

### `base.gen_<N>.ndb`

```
[64-byte header fields][u32 header CRC]
[document bytes, packed in slot (id) order]
[directory section]
    [u64 entry_off_0 .. u64 entry_off_{doc_count-1}]      fixed-stride offset table
    [u16 id_len][id][u64 doc_seq][u64 data_offset][u32 data_len]   per slot, packed
    [u32 dir CRC32]
[index section]
    u32 index_count
    per index:
      u8 kind, u16 field_len, field
      u32 entry_count
      pad to 8-byte file alignment
      [u64 entry_off_0 .. u64 entry_off_{K-1}]             fixed-stride offset table
      entries, each 4-byte aligned:
        [u32 key_len][encoded OrderedValue][pad to 4][u32 slot_count][u32 slot..]
    [u32 index CRC32]
```

The offset tables are what make the engine zero-copy: `MmapBase::entry_by_slot`
and `doc_bytes_by_slot` are O(1), `find_slot` is an O(log n) binary search over
id slices, and every index probe is a binary search over encoded key bytes
compared with the same `OrderedValue` total order. Slot arrays are returned as
`&[u32]` views into the mapping (entries are aligned for it), so no query
allocates per index entry. Encoded keys are never decoded to probe them.

## In-memory shape

Resting documents are **not** heap `serde_json::Value` trees and are **not**
mirrored in a per-document map:

```rust
struct V2State {
    manifest: V2Manifest,
    base: Arc<MmapBase>,          // the mapping; documents behind it
    superseded: Vec<u64>,         // one bit per base slot (12.5 KB per 100k docs)
    suppressed: u32,              // popcount of the bitset
    wal_overlay: HashMap<Box<str>, WalEntry>, // post-compaction writes only
    deleted: HashSet<String>,
    lineage: SourceLineage,
}
```

A cold open with an empty WAL allocates **nothing per document**. `get` checks
the WAL overlay, then binary-searches the mapped directory and parses exactly one
document. Indexes are never decoded into heap maps on open: the persisted
sections are queried in place, and only the small WAL overlay gets an in-memory
index (empty at cold boot). An index created after the last compaction has no
persisted section yet, so it is held as a full in-memory index until the next
compaction folds it into the base.

## Query path

- `query` / `query_with` / `query_projected` plan candidates through the shared
  planner, then evaluate the filter on the **document bytes** (`src/v2_json_scan.rs`,
  zero allocation per field) and materialize only what they return.
- `sort_by` + `limit` is a bounded top-K over sort keys read from raw bytes, so a
  100k-document scan retains `offset + limit` rows, not 100k.
- `fields: [...]` projects by parsing only the requested slices.
- `POST /v2/dbs/<name>/query` without `fields` writes the stored document bytes
  straight into the response body (`Database::query_raw`).
- Repeated reads are served from a bounded parsed-document cache (8 192 entries,
  validated by `doc_seq`, so an update can never be served stale).

## Writes and group commit

Every mutating call (`insert`, `update`, `set`, `remove`, `array_push`,
`apply_ops`, `delete`, `restore`) queues a `Job` on the per-database queue and
waits for a reply. The first free thread becomes leader, drains the queue, and
under the commit lock:

1. stages each mutation in FIFO order against a per-batch overlay (so `if_seq`
   and multi-op `apply_ops` see earlier jobs in the same batch),
2. encodes the resulting `WalRecord`s,
3. writes and fsyncs the whole batch in **one** `append_batch_and_sync`,
4. applies slots, indexes, ref counters, the change ring and `last_seq` — only
   after the fsync succeeded.

A failed fsync changes no RAM state and returns the error to every waiter.
`delete` writes and fsyncs its restorable `_trash/docs/*.jsonl` record before
its tombstone is staged (C5).

## Change feed

The in-RAM `change_ring` holds at most `V2_CHANGE_RING_CAP` (1024) events.
`changes_since(seq)` answers from RAM when the cursor is still inside the ring,
otherwise it streams `wal.gen_<N>.ndb` from disk via `scan_wal_since`. The
`CursorAhead` / `CursorExpired` contract and `source_epoch_uuid` are identical
to v1.

## Compaction

`compact()` is non-blocking for readers and writers:

1. **brief lock**: freeze `base_seq = last_seq`, `Arc::clone` the mapping, clone
   the superseded bitset and the small WAL overlay (a few KB — never the document
   set), take the declared index specs, create `wal.gen_<N+1>.ndb` with
   `base_seq = S` and install it as a **mirror** — later writes go to both WALs.
2. **no lock**: write `base.gen_<N+1>.ndb` (sorted docs + persisted Hash/BTree
   sections rebuilt with one raw field read per document per index), fsync, map
   and validate it.
3. **brief lock**: atomically rewrite `manifest.json`, swap the live
   `Arc<MmapBase>`, promote the mirror WAL, mark any post-freeze write's fresh
   base slot superseded (so it is counted once, from the overlay), rebuild the
   overlay indexes, trim the change ring to `> S`, unlink the superseded base and
   WAL (works while still mapped thanks to `FILE_SHARE_DELETE`).

Any failure in step 2 removes the mirror and the partial files, leaving the
live database untouched.

## CLI

```
ndb init <path> --v2            create a v2 folder database
ndb migrate <path>              v1 -> v2 (staged, verified, data.jsonl.v1.bak)
ndb fsck <path>                 v2: manifest/base/dir/index/WAL CRCs + bucket files
                                v1: journal + trash replay
ndb repair <path>               v1 journal truncation, or v2 WAL quarantine+truncate
ndb export <path> <out.jsonl> --jsonl
ndb compact <path>              works on both formats (takes the OS writer lock)
ndb info <path>                 reports Format: v1 or v2
```

`ndb migrate` stages everything in `<dir>/.migrating/` (a rerun replaces it),
verifies document-count and per-document canonical JSON parity against v1, then
publishes the generations, writes the manifest, renames `data.jsonl` to
`data.jsonl.v1.bak`, and removes the staging folder. `source_epoch_uuid` and
`last_seq` are preserved exactly.

## HTTP service

`ndb serve --config <file>` accepts `"format": "auto" | "v1" | "v2"` (also
`--format`), and `POST /v2/dbs` accepts `{"name": "...", "format": "v2"}` so a
service can create binary databases directly. `GET /v2/dbs` descriptors report
`"format": "v1" | "v2"`.

## Verification and measurement

- `tests/phase16_v2_storage_tests.rs` — every Phase 3 exit gate: torn WAL tail,
  mid-WAL corruption → `ndb repair`, group-commit writers + readers + mid-flight
  compaction (including Windows-safe unlink of superseded generations), the
  bounded change ring's WAL fallback past 1,024 events, persisted Hash/BTree
  indexes across a cold reopen, resumable `ndb migrate` (leftover `.migrating/`),
  real-corpus migration parity, and multi-DB `ndb serve` HTTP end-to-end on v2.
- `tests/phase15_perf_and_planner_tests.rs::planner_equals_full_scan_on_a_compacted_v2_database`
  — the 28-probe planner equivalence oracle against a compacted v2 database whose
  indexes are zero-copy mmap sections (also covers `query_raw` and writes through
  the WAL overlay after compaction).
- `benches/phase3_corpus_bench.rs --mode both|v1|v2` — the real-corpus v1 vs v2
  head-to-head. Results and the gate verdicts live in
  `docs/phase3-benchmark-v1-vs-v2.md`.
