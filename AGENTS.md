# nDB Project
.
## Project Goal

nDB is a high-performance, embedded, in-memory vector database designed for LLM workflows. It prioritizes reliability and performance over feature breadth, providing a minimal but complete API for vector storage and similarity search.

### Core Philosophy

- **Deterministic correctness**: Design failures away rather than handling them
- **Zero-cost abstractions**: Pay only for what you use
- **Instant recovery**: Memory-mapped persistence means large datasets don't require loading
- **Read-heavy optimization**: Readers never block; writers use append-only patterns

### Key Characteristics

| Attribute | Decision | Rationale |
|-----------|----------|-----------|
| **Language** | Rust | Memory safety, SIMD, no GC pauses, portable |
| **Deployment** | Embedded library | Single-node, application-integrated |
| **Storage** | LSM-Lite (memtable + mmap segments) + WAL | Instant writes, zero-copy reads, durability |
| **Serialization** | `rkyv` | True zero-copy deserialization |
| **Validation** | Checksum-once, trust-after | BLAKE3/CRC64 in header; validate on open only |
| **Index** | HNSW (configurable) | Industry-standard ANN, tunable recall |
| **SIMD** | `wide` crate | Stable Rust, portable (AVX2/AVX-512/NEON) |
| **API** | Mongo-like, minimal | Familiar but reduced surface area |
| **Durability** | Configurable | `Buffered` vs `FdatasyncEachBatch` |
| **Safety** | Multi-process locks | `flock` prevents dual-writer corruption |

### Target Workload

- **Primary use**: LLM embedding storage and retrieval
- **Vector dimensions**: 384-1536 per collection (optimized for 768, 1536)
- **Dataset size**: Limited only by available RAM
- **Access pattern**: Read-heavy, occasional bulk writes
- **Query types**: Exact (brute-force) and approximate (HNSW) with optional metadata filtering

### Collections

Databases support multiple collections, each with its own dimension. This allows:
- Multiple embedding models in one database
- Migration path when switching models (create new collection, re-embed)

```rust
let db = Database::open("data/")?;
let collection = db.create_collection("embeddings_v2", CollectionConfig { dim: 1536 })?;
```

### Data Model

```rust
// A document has a string ID, vector, and optional JSON payload
// Internally mapped to u32 for efficiency
struct Document {
    id: String,                    // Unique within collection
    vector: Vec<f32>,              // Embedding vector (fixed per collection)
    payload: Option<JsonValue>,    // Arbitrary metadata for filtering
}

// Search query
struct Search<'a> {
    vector: &'a [f32],             // Query vector (borrowed)
    top_k: usize,                  // Number of results
    filter: Option<Filter>,        // Metadata predicate (optional)
    approximate: bool,             // Use HNSW (true) or brute-force (false)
    ef: Option<usize>,             // HNSW quality parameter
}

// Search result
struct Match {
    id: String,
    score: f32,                    // Similarity score (1.0 = identical)
    payload: Option<JsonValue>,
}
```

### API Surface

```rust
// Database lifecycle
let db = Database::open(path)?;              // Open or create database

// Collection management
let coll = db.create_collection(name, config)?;  // New collection
let coll = db.get_collection(name)?;             // Existing collection

// Document operations
coll.insert(document)?;                      // Add or replace by ID
coll.insert_batch(documents)?;               // Bulk insert (single WAL entry)
coll.get(id)?;                               // Retrieve by ID
coll.delete(id)?;                            // Soft delete (WAL + bitmap)
coll.flush()?;                               // Freeze memtable → write to segment
coll.compact()?;                             // Physical removal + index rebuild (sync)
coll.sync()?;                                // Ensure WAL durability

// Search
let results = coll.search(query)?;           // Returns Vec<Match>
```

### Durability Contract

```rust
pub enum Durability {
    /// Acknowledge after append to OS page cache.
    /// Fastest, but data loss window is ~5-30 seconds (OS dependent).
    /// Use for high-throughput ingestion where recent data loss is acceptable.
    Buffered,
    
    /// Acknowledge after fdatasync() completes.
    /// Data is on disk before returning. Slower, but no data loss on crash.
    /// Use when durability is critical.
    FdatasyncEachBatch,
}
```

### Persistence Strategy

```
data/
├── MANIFEST                # Database-level: collections list
├── {collection_name}/
│   ├── MANIFEST            # Collection-level: active segments, WAL seq
│   ├── LOCK                # flock exclusive lock file
│   ├── segments/
│   │   ├── 0001.ndb        # Immutable mmap segment
│   │   └── 0002.ndb.tmp    # Compaction in progress (ignored)
│   ├── wal.log             # Append-only write log
│   └── index.hnsw          # Serialized HNSW graph
```

**Storage Model (LSM-Lite):**

| Component | Purpose | Mutability |
|-----------|---------|------------|
| **Memtable** | Recent writes in RAM | Mutable (WAL-backed) |
| **Segments** | Immutable historical data | Read-only, mmap'd |
| **WAL** | Durability log | Append-only |
| **HNSW** | ANN index | Rebuilt on compaction |
| **Manifest** | Atomic state transitions | Atomic rename only |

**Write Path:**
1. Append to WAL (fdatasync if `Durability::FdatasyncEachBatch`)
2. Insert into memtable (`HashMap` + SoA buffer)
3. Return immediately
4. When WAL exceeds threshold (64MB) or `flush()` called:
   freeze memtable → write to new segment → update manifest → reset WAL

**Read Path:**
1. Search memtable (HashMap lookup or SoA scan)
2. Search all segments (mmap'd, zero-copy)
3. Merge results

**Recovery:**
1. Read collection manifest for active segments and last WAL sequence
2. Mmap segments (instant, no parsing)
3. Replay WAL from `last_applied_seq + 1`
4. Reconstruct delete bitmap from WAL

**Compaction (Synchronous):**
1. Read manifest for active segments
2. Merge segments, remove deleted docs, rebuild HNSW
3. Write new segment(s) to `*.tmp`
4. Write new manifest atomically (atomic rename)
5. Delete old segments

If compaction crashes: orphan temp files ignored on next startup; old segments remain valid via old manifest.

### File Format

**Segment Header (64 bytes, aligned):**
```
[4]   magic: "nDB\0"
[2]   version: u16
[4]   dimension: u32
[8]   doc_count: u64
[8]   vector_offset: u64      // 64-byte aligned
[8]   id_mapping_offset: u64
[8]   payload_offset: u64
[8]   checksum: u64           // BLAKE3 truncated or CRC64 of body
[8]   reserved
```

**Segment Body Layout:**
```
[Vector Data Region]      // Packed f32s, 64-byte aligned, contiguous
[Internal ID Mapping]     // Vec<(u32, String)> for ID translation
[Payload Region]          // rkyv-serialized JSON blobs
```

Vector dimensions (384, 768, 1536) × 4 bytes are naturally 64-byte aligned.

**WAL Record:**
```
[8]   sequence: u64           // Monotonic per collection
[4]   record_len: u32         // Total bytes after this field
[4]   crc32: u32              // Covers sequence + body
[1]   opcode: u8              // Insert=1, Delete=2
[...] body (id, vector, payload for Insert; id for Delete)
```

- Sequence number enables idempotent replay (skip if `seq <= last_applied`)
- Record length enables forward-skip past corrupt records
- `last_applied_seq` stored in collection manifest

### Multi-Process Safety

- Exclusive `flock` on `LOCK` file for writers
- Readers require no lock (immutable segments)
- Explicit error if writer already exists: `Error::CollectionLocked`
- Documented: "nDB collections support single writer, multiple readers across processes"

### mmap Safety

**Risk:** SIGBUS on storage error or file truncation.

**Mitigations:**
- Verify file size matches header before mmap exposure
- Pre-fault pages on open (`madvise(MADV_POPULATE_READ)`) to surface I/O errors early
- SIGBUS handler converts to recoverable error where possible
- Documented: "nDB assumes reliable storage; corruption may terminate process"

### rkyv Validation Policy

- **On segment open**: Verify header checksum; if matches, skip deep validation
- **If checksum mismatch**: Run full rkyv validation; reject or quarantine segment
- **Post-open access**: Unchecked (zero-copy pointer access)
- **Debug builds**: Enable rkyv checked access unconditionally

This amortizes validation cost across all reads while maintaining safety.

### Non-Goals

- Distributed operation (use application-layer sharding if needed)
- ACID transactions across multiple documents
- Complex query language (no joins, aggregations)
- Network protocol (embedded only)
- Schema migrations within a collection (fixed dimension at creation)
- Background/async compaction for v1.0 (synchronous only)
- Multi-writer per collection (single writer enforced)

### Success Criteria

1. Sub-millisecond exact search on 100K vectors (768-dim, p99)
2. Millisecond-scale approximate search on 10M vectors with >95% recall
3. Instant recovery regardless of dataset size (mmap, no parsing)
4. Zero data loss on crash with `Durability::FdatasyncEachBatch`
5. Clear data loss window documented for `Durability::Buffered`
6. Multi-collection support for embedding model migration
7. Single-writer safety enforced across processes

---

## Agent Instructions

### Documentation Maintenance

**When modifying code, you MUST update corresponding documentation:**

1. `docs/development-plan.md` - Update when changing phase deliverables, success criteria, or technical decisions. Add entries to the Decision Log.

2. `docs/test-documentation.md` - Update when:
   - Adding, removing, or modifying tests
   - Changing test data characteristics
   - Updating success criteria verification
   - Adding new test suites or phases
   
   Include the current timestamp when updating.

3. `AGENTS.md` - Update when:
   - Changing architectural decisions
   - Modifying file formats or protocols
   - Adding new constraints or non-goals

**Documentation that does not match code is a bug.**

---

*See `docs/development-plan.md` for detailed development phases.*

Note: Start the session with reading the "philosophy" document from the documentation MCP enpoint.