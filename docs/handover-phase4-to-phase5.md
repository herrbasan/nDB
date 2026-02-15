# Handover: Phase 4 → Phase 5

**Date:** 2026-02-14  
**Status:** Phase 4 COMPLETE → Phase 5 READY  
**Tests:** 140 passing (93 unit + 47 integration)

---

## Phase 4 Summary (What Was Built)

### New Modules

| Module | Purpose | Key Types/Functions |
|--------|---------|---------------------|
| `src/filter.rs` | Mongo-like Filter DSL | `Filter` enum with Eq, Gt, Gte, Lt, Lte, In, And, Or |
| `tests/phase4_filter_tests.rs` | Integration tests | 13 tests for filter DSL |

### Modified Files

| File | Changes |
|------|---------|
| `src/search.rs` | Added `filter: Option<Filter>` to `Search`, `scan_memtable()`, `scan_segment()` with filtering |
| `src/lib.rs` | Added `filter` module, updated `flush()` to preserve payloads, updated `rebuild_index()` to include memtable |
| `src/memtable.rs` | Added `iter_active_with_payload()` method to `FrozenMemtable` |

### API

```rust
// Filter DSL (NEW)
let filter = Filter::eq("category", "books");
let filter = Filter::gt("year", 2020);
let filter = Filter::and([
    Filter::eq("status", "active"),
    Filter::gt("score", 4.5),
]);
let filter = Filter::eq("user.name", "alice");  // Nested field access

// Search with filter (NEW)
let results = coll.search(
    Search::new(&query_vec)
        .top_k(10)
        .filter(Filter::eq("category", "books"))
)?;
```

### Key Design Decisions

1. **Post-filtering strategy**: Filter is applied after vector search (not before)
   - Simpler implementation
   - May return fewer than `top_k` results if filter is selective
   - Guaranteed recall (no false negatives from filter)

2. **Missing field behavior**: Filter fails (document excluded) if referenced field doesn't exist
   - Explicit is better than implicit
   - MongoDB-compatible behavior

3. **Numeric type coercion**: Integers and floats are comparable
   - `Filter::eq("count", 5)` matches `{"count": 5.0}`
   - Uses `serde_json::Number::as_f64()` for comparison

4. **Dot notation for nested fields**: `"user.name"` accesses nested JSON
   - MongoDB-compatible syntax
   - Simple implementation using `split('.')`

---

## Phase 5: Compaction (Next)

### Goal
Implement synchronous compaction to reclaim space from deleted documents and rebuild the HNSW index.

### Deliverables

1. **Compaction API**
   ```rust
   pub fn compact(&self) -> Result<()> {
       // Merge all segments
       // Remove deleted documents
       // Rebuild HNSW index
       // Atomic manifest update
   }
   ```

2. **Statistics API** (optional for v1.0)
   ```rust
   pub fn info(&self) -> Result<CollectionInfo> {
       // doc_count, deleted_count, segment_count, index_size
   }
   ```

3. **Crash Safety**
   - Orphan temp files ignored on startup
   - Old segments valid via old manifest if compaction interrupted
   - Compaction idempotent (safe to retry)

### Success Criteria

| Metric | Target | How to Verify |
|--------|--------|---------------|
| Space reclaimed | 50% deletes → 50% size reduction | Test with known data |
| Query performance | Maintained or improved | Benchmark before/after |
| Crash recovery | No data loss | Simulate crashes |

---

## Entry Points for Phase 5

### Files to Modify

1. **`src/lib.rs`**
   - Add `compact()` method to `Collection`
   - Add `info()` method (optional)

2. **`src/manifest.rs`** (if needed)
   - Track compaction generation

3. **New file: `src/compaction.rs`** (optional)
   - Separate module for compaction logic

### Key Algorithm

```rust
fn compact(&self) -> Result<()> {
    // 1. Read manifest for active segments
    let segments = self.segments.load();
    
    // 2. Merge segments, filtering out deleted docs
    let mut merged = Vec::new();
    for segment in segments.iter() {
        for doc in segment.iter() {
            if !is_deleted(doc.id) {
                merged.push(doc);
            }
        }
    }
    
    // 3. Write new segment(s) to *.tmp
    let new_segment_path = write_to_temp(merged)?;
    
    // 4. Build new HNSW index
    let new_index = build_hnsw(&merged)?;
    let new_index_path = write_index_to_temp(new_index)?;
    
    // 5. Update manifest atomically
    let mut manifest = self.manifest.lock()?;
    manifest.clear_segments();
    manifest.add_segment(new_segment_path);
    manifest.set_index_file(new_index_path);
    manifest.save()?;  // Atomic rename
    
    // 6. Delete old segments (after manifest is updated)
    for old_segment in segments.iter() {
        std::fs::remove_file(&old_segment.path())?;
    }
    
    // 7. Update in-memory segments list
    self.segments.store(Arc::new(vec![new_segment]));
    
    Ok(())
}
```

---

## Testing Strategy for Phase 5

### Unit Tests (`src/lib.rs` or `src/compaction.rs`)

| Test | Purpose |
|------|---------|
| `test_compact_empty` | Compact empty collection |
| `test_compact_no_deletes` | Verify no change when no deletes |
| `test_compact_with_deletes` | Verify deleted docs removed |
| `test_compact_preserves_data` | All non-deleted docs preserved |

### Integration Tests (`tests/phase5_compaction_tests.rs`)

| Test | Purpose |
|------|---------|
| `test_compaction_reduces_size` | File size reduces appropriately |
| `test_compaction_rebuilds_index` | HNSW index rebuilt |
| `test_compaction_crash_recovery` | Orphan files handled |
| `test_compaction_query_after` | Search works after compaction |

---

## Open Questions

1. **Segment size target**: Should we aim for a single segment after compaction, or multiple?
   - Recommendation: Single segment for simplicity v1.0

2. **Compaction trigger**: Should compaction be automatic or manual only?
   - Recommendation: Manual only for v1.0 (`compact()` is explicit)

3. **Progress tracking**: Should compaction report progress for large collections?
   - Recommendation: Optional for v1.0

---

## Quick Start Commands

```bash
# Verify Phase 4 still works
cargo test

# Create Phase 5 test scaffold
touch tests/phase5_compaction_tests.rs

# Run just compaction tests (once written)
cargo test --test phase5_compaction_tests
```

---

## Current Test Count

| Phase | Tests |
|-------|-------|
| Unit | 93 |
| Phase 1A | 16 (7 locking + 9 segment) |
| Phase 2 | 11 |
| Phase 3 | 7 |
| Phase 4 | 13 |
| **Total** | **140** |

---

## Checklist for Phase 5 Completion

- [ ] `compact()` method on Collection
- [ ] Deleted documents physically removed
- [ ] HNSW index rebuilt after compaction
- [ ] Atomic manifest update during compaction
- [ ] Orphan temp file cleanup on startup
- [ ] Unit tests for compaction logic
- [ ] Integration tests with crash simulation
- [ ] Documentation update

---

*See `docs/development-plan.md` for detailed Phase 5 requirements.*
