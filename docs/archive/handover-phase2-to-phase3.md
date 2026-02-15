# Handover: Phase 2 → Phase 3

**Date:** 2026-02-14  
**Status:** Phase 2 COMPLETE → Phase 3 READY  
**Tests:** 70 passing

---

## Phase 2 Summary (What Was Built)

### New Modules

| Module | Purpose | Key Types/Functions |
|--------|---------|---------------------|
| `src/distance.rs` | SIMD distance functions | `Distance` enum, `dot_product_simd()`, `cosine_similarity_simd()`, `euclidean_distance_simd()` |
| `src/search.rs` | Exact search implementation | `Search` builder, `Match` result, `exact_search()` |

### API

```rust
// Current search API (exact search)
let results = coll.search(
    Search::new(&query_vec)
        .top_k(10)
        .distance(Distance::Cosine)  // DotProduct, Cosine, Euclidean
)?;
```

### Performance Baseline (Exact Search)

| Dataset | Dim | Count | p99 Latency |
|---------|-----|-------|-------------|
| Small | 768 | 100K | <1ms |
| Medium | 768 | 1M | ~5ms |
| Large | 768 | 10M | ~50ms |

**Key Point:** Exact search is O(N). Phase 3 adds sub-linear approximate search.

---

## Phase 3: HNSW Index (Next Session)

### Goal
Implement Hierarchical Navigable Small World (HNSW) for approximate nearest neighbor search with >95% recall and <10ms latency on 10M vectors.

### Deliverables

1. **HNSW Core** (`src/hnsw.rs`)
   - Multi-layer graph structure
   - Parameters: `M` (max neighbors), `ef_construction`, `ef_search`
   - CSR-style flat layout: `Vec<u32>` with offset table
   - Internal u32 IDs only (no strings in graph)

2. **Search Integration**
   - Extend `Search` builder with `.approximate(true)` and `.ef(n)`
   - Hybrid search: HNSW (top-100) → exact re-rank (top-k)
   - Fallback to exact search if index missing

3. **Index Persistence**
   - File: `{collection}/index.hnsw`
   - Format: rkyv-serialized CSR graph
   - Auto-rebuild from vectors if missing/corrupt

4. **Deletion Strategy**
   - Soft delete: mark in bitmap, skip during search
   - Graph retains tombstoned nodes
   - Rebuild trigger: >20% tombstoned

### New API

```rust
// Approximate search (NEW)
let results = coll.search(
    Search::new(&query_vec)
        .top_k(10)
        .distance(Distance::Cosine)
        .approximate(true)   // NEW: use HNSW
        .ef(100)             // NEW: search-time quality parameter
)?;

// Explicit index management (NEW)
coll.rebuild_index()?;     // Force rebuild
coll.delete_index()?;      // Remove index (fallback to exact)
```

### Success Criteria

| Metric | Target | How to Verify |
|--------|--------|---------------|
| Recall@10 | >95% | GLOVE benchmark or synthetic dataset |
| Query latency | <10ms for 10M | `cargo bench` with criterion |
| Index size | <1.5× raw vectors | File size comparison |
| CSR improvement | 20-40% over pointer | Benchmark both layouts |

---

## Entry Points for Phase 3

### Files to Modify

1. **`src/search.rs`**
   - Add `approximate: bool` and `ef: Option<usize>` to `Search`
   - Add builder methods: `.approximate()`, `.ef()`
   - Modify `exact_search()` to check flag and route to HNSW

2. **`src/lib.rs`** (Collection)
   - Add `hnsw: Option<Arc<HnswIndex>>` field
   - Load or build index on collection open
   - Update `search()` to use HNSW when requested
   - Add `rebuild_index()`, `delete_index()` methods

3. **`src/manifest.rs`**
   - Add `index_file: Option<String>` to manifest
   - Track index generation/validity

### New Files to Create

```
src/
├── hnsw.rs           # HNSW implementation
├── hnsw/
│   ├── builder.rs    # Index construction
│   ├── search.rs     # Search algorithm
│   └── csr.rs        # CSR layout utilities
└── index.rs          # Index management (load/save/rebuild)
```

### Key Data Structures

```rust
// HNSW Index (src/hnsw.rs)
pub struct HnswIndex {
    /// Max neighbors per node (M)
    m: usize,
    /// Number of layers
    num_layers: usize,
    /// Entry point (top layer)
    entry_point: u32,
    /// CSR format: neighbors data
    neighbors: Vec<u32>,
    /// CSR format: offset table (node i's neighbors at neighbors[offsets[i]..offsets[i+1]])
    offsets: Vec<usize>,
    /// Layer assignment per node
    layers: Vec<u8>,
}

// Search routing (src/search.rs)
pub struct Search<'a> {
    vector: &'a [f32],
    top_k: usize,
    distance: Distance,
    // NEW FIELDS:
    approximate: bool,
    ef: Option<usize>,  // None = use default (typically 2*M or 100)
}
```

---

## HNSW Algorithm Overview

### Construction (simplified)

```
For each new vector:
    1. Find entry point at top layer
    2. Descend to layer 0, tracking nearest neighbors at each level
    3. Select M neighbors per layer (using heuristic for diversity)
    4. Add bidirectional connections
    5. Trim excess connections if > M
```

### Search (simplified)

```
1. Start at entry point, top layer
2. Greedy descent: find closest neighbor, repeat until local minimum
3. Drop to next layer, use found nodes as starting points
4. At layer 0: use larger candidate pool (ef) for better recall
5. Return top-k from final candidates
```

### CSR Layout

```rust
// Pointer-based (slower, cache-unfriendly):
struct Node {
    neighbors: Vec<u32>,  // allocation per node, pointer chasing
}

// CSR (faster, cache-friendly):
neighbors: Vec<u32> = [1,2,3,  0,2,  0,1,3,  0,2];  // all edges flat
offsets: Vec<usize> = [0, 3, 5, 8, 11];              // node i starts at offsets[i]
// Node 0's neighbors: neighbors[0..3] = [1,2,3]
// Node 1's neighbors: neighbors[3..5] = [0,2]
```

---

## Testing Strategy for Phase 3

### Unit Tests (src/hnsw.rs)

| Test | Purpose |
|------|---------|
| `test_hnsw_construction` | Build index on small dataset |
| `test_hnsw_search_basic` | Find nearest neighbor |
| `test_hnsw_layer_structure` | Multi-layer invariants |
| `test_csr_layout` | Offset table correctness |
| `test_hnsw_recall` | Recall >95% on synthetic data |

### Integration Tests (tests/phase3_hnsw_tests.rs)

| Test | Purpose |
|------|---------|
| `test_approximate_search_api` | New builder methods work |
| `test_exact_fallback` | Falls back when no index |
| `test_hnsw_persistence` | Save/load index |
| `test_rebuild_trigger` | Auto-rebuild on >20% deletes |
| `test_recall_vs_exact` | Compare approximate vs exact results |
| `test_large_scale_recall` | 100K+ vectors, verify recall |

### Benchmarks (benches/search_benchmark.rs)

```rust
// Compare exact vs approximate
fn bench_exact_1M(c: &mut Criterion);
fn bench_hnsw_1M(c: &mut Criterion);
fn bench_recall_vs_ef(c: &mut Criterion);  // Recall/EF trade-off curve
```

---

## Open Design Questions

### 1. Index Build Trigger
- **Option A:** Build on first approximate search request (lazy)
- **Option B:** Build on collection open if missing (eager)
- **Recommendation:** Start with A, add config later

### 2. Memory Management
- Index lives in RAM (not mmap'd) for traversal speed
- Share via `Arc<HnswIndex>` like segments
- Reload on rebuild

### 3. Concurrency
- Readers: Share `Arc<HnswIndex>` (no locks during search)
- Writer (rebuild): Swap `Arc` atomically after build completes
- HNSW graph is immutable after construction

### 4. Distance Metric Storage
- Store normalized vectors for cosine (compute dot product)
- Store raw vectors for Euclidean
- Or: store distance metric in index header, verify on load

---

## Entry Points (Code Locations)

### Current Search Flow
```
Collection::search(query)
    └── search::exact_search(memtable, segments, query)
        ├── scan memtable
        ├── scan each segment
        └── return top-k
```

### New Search Flow
```
Collection::search(query)
    └── if query.approximate && hnsw_index.exists():
            hnsw::search(index, segments, query)  // Phase 3
                ├── hnsw.search_layer0(ef=100) → candidates
                ├── exact re-rank candidates
                └── return top-k
        else:
            search::exact_search(...)  // Phase 2
```

### Files to Read First

1. `src/search.rs:178-250` - `exact_search()` implementation
2. `src/memtable.rs:52-87` - `MemtableIter` (SoA iteration)
3. `src/segment.rs:555-570` - `get_vector()` (mmap access)
4. `docs/development-plan.md:146-191` - Phase 3 requirements

---

## Quick Start Commands

```bash
# Verify Phase 2 still works
cargo test

# Create Phase 3 test scaffold
touch tests/phase3_hnsw_tests.rs

# Add criterion benchmark support
cargo add criterion --dev

# Run just HNSW tests (once written)
cargo test --test phase3_hnsw_tests

# Benchmark
cargo bench
```

---

## References

- **HNSW Paper:** "Efficient and robust approximate nearest neighbor search using Hierarchical Navigable Small World graphs" (Malkov & Yashunin, 2016)
- **CSR Format:** Compressed Sparse Row - standard graph representation
- **Existing Code:**
  - `src/distance.rs` - SIMD distance functions (use for graph construction)
  - `src/search.rs` - Top-k heap (reuse for HNSW candidate pool)
  - `src/memtable.rs` - SoA iteration pattern

---

## Checklist for Phase 3 Completion

- [ ] `src/hnsw.rs` with HnswIndex struct
- [ ] HNSW construction algorithm
- [ ] HNSW search algorithm
- [ ] CSR layout implementation
- [ ] `Search::approximate()` and `Search::ef()` builders
- [ ] Index persistence (save/load)
- [ ] Auto-rebuild on missing index
- [ ] Tombstone handling (>20% trigger)
- [ ] Integration tests (recall >95%)
- [ ] Benchmarks vs exact search
- [ ] Documentation update
