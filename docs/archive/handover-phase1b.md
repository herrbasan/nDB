# Handover: Phase 1B Complete

**Date:** 2026-02-14  
**Status:** Phase 1B (WAL, Memtable & Recovery) - COMPLETE  
**Tests:** 58 passing (41 unit + 16 integration + 1 doc)

---

## What Was Built

### New Modules
1. **`src/wal.rs`** - Write-ahead log with CRC32 checksums, sequence numbers, automatic truncation of corrupt tails
2. **`src/manifest.rs`** - Atomic collection manifest (JSON + rename), tracks segments and WAL position
3. **`src/memtable.rs`** - In-memory storage: HashMap for O(1) lookups + SoA vector buffer for SIMD scans

### Collection API (src/lib.rs)
```rust
// Database
let db = Database::open(path)?;
let coll = db.create_collection("name", CollectionConfig { dim: 768 })?;
let coll = db.get_collection("name")?;

// Collection
coll.insert(Document { id, vector, payload })?;
coll.insert_batch(docs)?;  // Single WAL sync for durability mode
coll.get(id)?;             // Memtable → segments (newest first)
coll.delete(id)?;          // Soft delete + WAL record
coll.flush()?;             // Memtable → segment, reset WAL
coll.sync()?;              // Explicit fdatasync
```

---

## Key Design Decisions

| Decision | Rationale |
|----------|-----------|
| `last_wal_seq` only updated on flush | Prevents WAL records from being skipped during recovery |
| Recovery replays `last_wal_seq+1` to `next_seq-1` | Handles all unflushed writes since last checkpoint |
| Lock passed `create_collection` → `open_with_lock` | Avoids double-lock failure |
| SoA vector layout | Enables future SIMD optimization |
| Synchronous flush only | Phase 5 will add background compaction |

---

## Architecture Reminder

```
data/
├── MANIFEST                    # Database-level: collection names
└── {collection}/
    ├── MANIFEST                # Tracks segments, last_wal_seq
    ├── LOCK                    # flock exclusive lock
    ├── wal.log                 # Append-only WAL
    └── segments/
        └── 0001.ndb            # Immutable mmap segment
```

**Write Path:** WAL append → memtable insert → check_flush() → optional flush  
**Read Path:** Memtable → segments (newest first)  
**Recovery:** Replay WAL from `last_wal_seq + 1`

---

## Current Limitations (Known)

- Payloads not persisted in memtable flush (simplified)
- No background/async operations (all sync)
- No compaction (Phase 5)
- No search/index (Phase 2 & 3)

---

## Next: Phase 2 - Exact Similarity

Per dev plan, build brute-force vector search with SIMD:

1. **SIMD distance functions** (`wide` crate)
   - Dot product, cosine similarity, Euclidean
   - Aligned loads (vectors are 64-byte aligned in segment)

2. **Exact search implementation**
   - Linear scan over memtable (SoA) + all segments
   - Top-k selection via binary heap
   - Deterministic tie-break: score, then internal ID

3. **Benchmark protocol**
   - p50/p95/p99 latency
   - SIMD vs scalar comparison

**API Addition:**
```rust
pub enum Distance { DotProduct, Cosine, Euclidean }
let results = coll.search(Search::new(&query_vec).top_k(10).distance(Distance::Cosine))?;
```

---

## Files to Review

- `docs/phase1b-summary.md` - Detailed implementation notes
- `docs/development-plan.md` - Phase 2 section (line ~100)
- `src/memtable.rs:84-118` - SoA iterator (ready for SIMD)
- `src/segment.rs:555-570` - get_vector() (zero-copy mmap)

---

## Test Everything Still Works

```bash
cargo test  # 58 tests should pass
```
