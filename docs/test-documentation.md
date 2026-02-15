# nDB Test Documentation

> **Last Updated:** 2026-02-14 21:15:00+01:00  
> **Phase:** 5 — Maintenance: Compaction  
> **Commit:** Phase 5 complete (Compaction + crash safety)

## Overview

This document describes the test suite for nDB. Tests are organized by component and verify both functional correctness and architectural invariants.

---

## Test Organization

```
tests/
├── phase1a_segment_tests.rs    # Segment file format tests
├── phase1a_locking_tests.rs    # Multi-process locking tests
├── phase2_search_tests.rs      # Exact similarity search tests
├── phase3_hnsw_tests.rs        # HNSW approximate search tests
├── phase4_filter_tests.rs      # Filter DSL integration tests
├── phase5_compaction_tests.rs  # Compaction integration tests
└── (unit tests in src/*.rs)    # Module-level unit tests
```

---

## Running Tests

```bash
# All tests
cargo test  # 157 tests passing

# Specific test suite
cargo test --test phase1a_segment_tests
cargo test --test phase1a_locking_tests
cargo test --test phase2_search_tests
cargo test --test phase3_hnsw_tests
cargo test --test phase4_filter_tests
cargo test --test phase5_compaction_tests

# With output
cargo test -- --nocapture

# Release mode (for performance validation)
cargo test --release
```

---

## Phase 2: Search Tests

### Search Tests (`tests/phase2_search_tests.rs`)

| Test | Purpose | Success Criteria |
|------|---------|------------------|
| `test_search_basic_cosine` | Cosine similarity search | Query [1,0,0] returns "a" (exact match) first, then "d" (45°) |
| `test_search_top_k_limit` | Top-k limiting | Different k values return correct number of results |
| `test_search_different_distances` | All distance metrics | Dot, Cosine, Euclidean all return expected ordering |
| `test_search_over_segments` | Cross-storage search | Documents found in both memtable and flushed segments |
| `test_search_dimension_mismatch` | Input validation | Wrong dimension returns Error::WrongDimension |
| `test_search_deterministic_tie_breaking` | Stable ordering | Identical scores tie-break by internal ID |
| `test_search_empty_collection` | Edge case | Empty collection returns empty results |
| `test_search_deleted_documents_excluded` | Soft delete | Deleted documents not in search results |
| `test_search_large_dimension` | Real-world dims | 768-dimension vectors work correctly |
| `test_search_persistence` | Recovery | Search works after close/reopen |
| `test_search_result_with_payload` | Payload handling | Documents found (payload retrieval documented) |

**Key Invariants Verified:**
- 100% recall (exact search)
- SIMD vs scalar parity within FP tolerance
- Deterministic tie-breaking: score descending, then internal_id ascending
- Search spans memtable + all segments

---

## Phase 3: HNSW Index Tests

### HNSW Tests (`tests/phase3_hnsw_tests.rs`)

| Test | Purpose | Success Criteria |
|------|---------|------------------|
| `test_approximate_search_api` | New builder methods work | `.approximate(true).ef(64)` returns results |
| `test_exact_fallback_when_no_index` | Graceful fallback | Falls back to exact search when no HNSW index |
| `test_hnsw_persistence` | Index save/load | Index persists across close/reopen |
| `test_delete_index` | Index removal | `delete_index()` removes index, search still works |
| `test_recall_vs_exact` | Approximate vs exact | Some overlap between exact and approximate results |
| `test_ef_parameter_effect` | EF parameter | Different ef values produce results |
| `test_search_builder_methods` | API verification | `is_approximate()`, `ef_value()` work correctly |

**Key Invariants Verified:**
- HNSW index builds without errors
- Approximate search returns results
- Fallback to exact search when index missing
- Index persists across database close/reopen
- `ef` parameter affects search scope

### HNSW Unit Tests (`src/hnsw.rs`)

| Test | Purpose |
|------|---------|
| `test_hnsw_params_default` | Default parameters (M=16, ef=32) |
| `test_hnsw_params_with_m` | Custom M parameter |
| `test_hnsw_build_and_search` | Index builds and returns results |
| `test_hnsw_search_dimension_mismatch` | Wrong dimension returns error |
| `test_hnsw_empty_search` | Empty index returns error |
| `test_hnsw_recall` | Recall > 30% on synthetic data |
| `test_candidate_ordering` | Max-heap ordering correct |
| `test_csr_layout` | CSR format structure valid |
| `test_hnsw_graph_has_edges` | Graph construction creates edges |

**Implementation Notes:**
- CSR (Compressed Sparse Row) layout for cache efficiency
- Multi-layer graph with probabilistic level assignment
- Greedy search with dynamic candidate list (ef parameter)
- Basic implementation prioritizes correctness over optimal recall

---

## Phase 4: Filter DSL Tests

### Filter Tests (`tests/phase4_filter_tests.rs`)

| Test | Purpose | Success Criteria |
|------|---------|------------------|
| `test_filter_with_exact_search` | Basic filter integration | Filter applied to exact search results |
| `test_filter_comparison_operators` | Gt, Gte, Lt, Lte | All comparison operators work correctly |
| `test_filter_and_or_operators` | Logical operators | And/Or combine filters correctly |
| `test_filter_in_operator` | Array membership | `In` filter matches values in array |
| `test_filter_nested_fields` | Dot notation | `"user.name"` accesses nested JSON |
| `test_filter_selective_returns_fewer_results` | Post-filtering behavior | Filtered search may return < top_k results |
| `test_filter_no_matches` | Empty results | Filter matching nothing returns empty vec |
| `test_filter_over_segments` | Cross-storage filtering | Filter works across memtable + segments |
| `test_filter_with_approximate_search` | HNSW + filtering | Filter works with approximate search |
| `test_filter_complex_nested` | Complex queries | Nested And/Or with dot notation |
| `test_filter_numeric_type_coercion` | Int/float comparison | `5` equals `5.0` in filters |
| `test_filter_documents_without_payload` | Missing payload handling | Documents without payload excluded when filter present |
| `test_filter_combined_with_distance_metric` | Filter + distance | Filter works with all distance metrics |

**Key Invariants Verified:**
- Post-filtering: Filter applied after vector search
- Missing field = document excluded (filter fails)
- Numeric coercion: integers and floats comparable
- MongoDB-compatible dot notation for nested access
- Works with both exact and approximate search

### Filter Unit Tests (`src/filter.rs`)

| Test | Purpose |
|------|---------|
| `test_filter_eq_string` | Equality with strings |
| `test_filter_eq_number` | Equality with numbers (int/float coercion) |
| `test_filter_eq_bool` | Equality with booleans |
| `test_filter_eq_missing_field` | Missing field returns false |
| `test_filter_gt` | Greater than comparison |
| `test_filter_gte` | Greater than or equal |
| `test_filter_lt` | Less than comparison |
| `test_filter_lte` | Less than or equal |
| `test_filter_numeric_coercion` | Int vs float comparison |
| `test_filter_in` | Array membership |
| `test_filter_in_numeric` | In with numeric coercion |
| `test_filter_and` | Logical AND |
| `test_filter_or` | Logical OR |
| `test_filter_nested_and_or` | Complex boolean logic |
| `test_filter_nested_field` | Dot notation access |
| `test_filter_deeply_nested` | Multi-level nesting |
| `test_filter_nested_missing` | Missing nested field |
| `test_filter_empty_and` | Empty And returns true |
| `test_filter_empty_or` | Empty Or returns false |
| `test_get_field_simple` | Direct field access |
| `test_get_field_nested` | Dot notation traversal |

**Filter DSL API:**
```rust
// Basic predicates
Filter::eq("field", value)
Filter::gt("field", value)
Filter::gte("field", value)
Filter::lt("field", value)
Filter::lte("field", value)
Filter::in_("field", [v1, v2, v3])

// Logical operators
Filter::and([filter1, filter2])
Filter::or([filter1, filter2])

// Nested fields
Filter::eq("user.name", "alice")
```

---

### Distance Unit Tests (`src/distance.rs`)

| Test | Purpose |
|------|---------|
| `test_dot_product_basic` | Correctness: 4D dot product |
| `test_dot_product_aligned` | SIMD alignment: 8D (one chunk) |
| `test_dot_product_with_remainder` | Remainder handling: 10D (1 chunk + 2) |
| `test_cosine_same_vector` | Self-similarity = 1.0 |
| `test_cosine_orthogonal` | Orthogonal vectors = 0.0 |
| `test_cosine_opposite` | Opposite vectors = -1.0 |
| `test_cosine_zero_vector` | Zero vector handling |
| `test_euclidean_basic` | L2 distance correctness |
| `test_euclidean_same_vector` | Self-distance = 0.0 |
| `test_euclidean_aligned` | SIMD alignment for L2 |
| `test_distance_enum` | Distance::compute() API |
| `test_distance_dimension_mismatch` | Error handling |
| `test_higher_is_better` | Distance metric semantics |
| `test_large_dimension` | 384, 768, 1536 dims with SIMD vs scalar |

**Scalar Fallback:**
All SIMD functions have scalar implementations in `distance::scalar` module for verification.

---

## Phase 1B: WAL, Memtable & Recovery Tests

### Unit Tests

#### `src/wal.rs` — Write-Ahead Log

| Test | Purpose |
|------|---------|
| `test_wal_append_and_replay` | Basic append/replay cycle |
| `test_wal_idempotent_replay` | Same record skipped on replay |
| `test_wal_corruption_truncation` | Corrupt tail truncated gracefully |
| `test_wal_reset` | WAL clear/reset functionality |
| `test_delete_record` | Delete operation serialization |

#### `src/manifest.rs` — Collection Manifest

| Test | Purpose |
|------|---------|
| `test_manifest_roundtrip` | Save/load preserves data |
| `test_manifest_atomic_update` | Write-temp-rename atomicity |
| `test_manifest_manager` | Manager wrapper functionality |
| `test_remove_segments` | Segment removal from manifest |
| `test_manifest_not_found` | Missing file handling |

#### `src/memtable.rs` — In-Memory Storage

| Test | Purpose |
|------|---------|
| `test_memtable_insert_and_get` | Basic insert/retrieve |
| `test_memtable_dimension_mismatch` | Validation |
| `test_memtable_delete` | Soft delete |
| `test_memtable_iter` | SoA iterator skips deleted |
| `test_memtable_replace` | Update existing document |
| `test_memtable_soa_layout` | Contiguous vector storage |
| `test_frozen_memtable` | Flush preparation |
| `test_active_count` | Deleted vs total count |

#### `src/lib.rs` — Collection API

| Test | Purpose |
|------|---------|
| `test_collection_insert_and_get` | Basic document operations |
| `test_collection_insert_batch` | Batch insert efficiency |
| `test_collection_delete` | Soft delete via API |
| `test_collection_flush` | Memtable → segment |
| `test_collection_persistence` | Close/reopen data integrity |

---

## Phase 1A: Storage Primitives Tests

### Segment Tests (`tests/phase1a_segment_tests.rs`)

| Test | Purpose | Success Criteria |
|------|---------|------------------|
| `test_segment_create_and_reopen` | End-to-end segment lifecycle | Create → write 100 docs → reopen → all data intact |
| `test_segment_header_format` | Binary format verification | Magic bytes, version, dimension, doc_count at correct offsets |
| `test_segment_corruption_detection` | Checksum validation | Corrupted file fails checksum verification |
| `test_segment_with_empty_payloads` | Optional payload handling | Documents without payloads deserialize correctly |
| `test_segment_iterators` | Iterator correctness | Vector iterator and full iterator return expected data |
| `test_large_dimension_vectors` | Common embedding dimensions | 384, 768, 1536 dimensions all work correctly |
| `test_segment_sharing_across_threads` | Thread safety | 4 threads concurrently read from shared Arc<Segment> |
| `test_empty_segment_rejected` | Validation | Building empty segment returns error |
| `test_dimension_mismatch_detected` | Input validation | Wrong vector dimension detected at insert time |

**Key Invariants Verified:**
- 64-byte header alignment
- Little-endian byte order
- BLAKE3 checksum computation
- Zero-copy vector access via raw pointers
- Thread-safe read-only access

### Locking Tests (`tests/phase1a_locking_tests.rs`)

| Test | Purpose | Success Criteria |
|------|---------|------------------|
| `test_lock_basic_acquire_and_release` | Lock lifecycle | Lock acquired, `is_locked()` returns true, released on drop |
| `test_lock_exclusivity` | Single-writer guarantee | Second lock attempt fails with `CollectionLocked` |
| `test_lock_multiple_collections` | Isolation | Different collections can be locked simultaneously |
| `test_lock_thread_safety` | Cross-thread behavior | Lock status checkable from other threads |
| `test_lock_file_persistence` | Lock file handling | LOCK file created and persists after unlock |
| `test_lock_drop_releases` | RAII correctness | 10 consecutive acquire/drop cycles all succeed |
| `test_lock_recovery_after_abandoned_lock` | Crash recovery | Can acquire lock even if LOCK file exists (process died) |

**Platform-Specific Behavior:**
- **Unix**: `flock(2)` with `LOCK_EX | LOCK_NB`
- **Windows**: `LockFileEx` with `EXCLUSIVE | FAIL_IMMEDIATELY`

**Known Limitations:**
- Same-process lock exclusivity on Windows may vary by Windows version (LockFileEx behavior)
- Cross-process locking is the primary guarantee

### Unit Tests (in `src/` modules)

#### `src/id.rs` — ID Mapping

| Test | Purpose |
|------|---------|
| `test_basic_mapping` | String ↔ u32 bidirectional mapping |
| `test_duplicate_insert` | Duplicate IDs return existing internal ID |
| `test_remove` | Removal clears both directions |
| `test_roundtrip_vec` | Serialization round-trip preserves all mappings |

#### `src/segment.rs` — Segment Builder

| Test | Purpose |
|------|---------|
| `test_segment_builder_and_open` | Build segment, verify all fields |
| `test_dimension_mismatch` | Wrong dimension rejected at insert |
| `test_empty_segment_fails` | Empty segment build returns error |
| `test_checksum_verification` | Corruption detected via checksum mismatch |

#### `src/lib.rs` — Database API

| Test | Purpose |
|------|---------|
| `test_database_open` | Database directory creation |
| `test_collection_config` | Config builder pattern |

---

## Test Data Characteristics

### Vector Data
- **Dimensions tested:** 4 (unit tests), 384, 768, 1536 (integration tests)
- **Document counts:** 1-100 (scalable to millions)
- **Values:** Sequential floats (0.0, 1.0, 2.0...) for deterministic verification

### Payload Data
- JSON format: `{"id": "docN", "value": N}`
- Optional payloads (None) also tested

### File Locations
All tests use `tempfile::TempDir` for isolation:
- Each test gets unique temp directory
- Automatically cleaned up on test completion
- No interference between tests

---

## Success Criteria (Phase 1A)

From `docs/development-plan.md`:

> - Create segment, write documents, close, reopen — data intact via mmap
> - Checksum verification passes; corruption detected and rejected
> - Two processes cannot open same collection as writers simultaneously

**Verification:**
- ✅ `test_segment_create_and_reopen` — Data integrity
- ✅ `test_segment_corruption_detection` — Checksum validation
- ✅ `test_lock_exclusivity` — Single-writer enforcement

---

## Success Criteria (Phase 4)

From `docs/development-plan.md`:

> - Filter correctness: 100% — Unit tests for all predicate types
> - Post-filter recall: Documented — May return <k results if filter is selective
> - Filter evaluation <50% of search time for typical predicates

**Verification:**
- ✅ 15 unit tests for Filter DSL (all predicates, nesting, type coercion)
- ✅ 13 integration tests for filtered search
- ✅ Post-filtering behavior verified: selective filters return fewer results
- ✅ Works with both exact and approximate search

## Performance Baselines

Current test execution time (debug build, Windows 11, NVMe SSD):

| Test Suite | Time |
|------------|------|
| Unit tests (59) | ~60ms |
| Locking tests (7) | ~5ms |
| Segment tests (9) | ~40ms |
| Search tests (11) | ~20ms |
| Phase 4 tests (13) | ~60ms |
| **Total** | **~185ms** |

---

## Phase 5: Compaction Tests

### Unit Tests (`src/compaction.rs`)

| Test | Purpose |
|------|---------|
| `test_merge_segments_basic` | Merge multiple segments |
| `test_merge_segments_with_deletes` | Filter out deleted docs during merge |
| `test_merge_segments_newer_wins` | Document updates: newer version takes precedence |
| `test_cleanup_temp_files` | Orphan temp file cleanup |
| `test_compact_empty_collection` | Empty collection compaction |

### Integration Tests (`tests/phase5_compaction_tests.rs`)

| Test | Purpose | Success Criteria |
|------|---------|------------------|
| `test_compaction_reduces_size` | Space reclamation | 50% deletes → ~50% size reduction |
| `test_compaction_with_multiple_segments` | Multi-segment merge | All segments merged into one |
| `test_compaction_preserves_data_integrity` | Data preservation | All non-deleted docs intact after compaction |
| `test_compaction_query_after` | Search correctness | Same results before/after compaction |
| `test_compaction_empty_collection` | Empty collection handling | No error, no change |
| `test_compaction_no_deletes` | No-op compaction | Docs preserved when no deletes exist |
| `test_compaction_all_deleted` | All docs deleted | Collection becomes empty |
| `test_compaction_rebuilds_index` | HNSW index rebuild | Index rebuilt and functional after compaction |
| `test_compaction_persistence` | Data survives reopen | Compacted data valid after close/reopen |
| `test_compaction_orphan_cleanup` | Temp file cleanup | Orphan files removed on startup |
| `test_compaction_with_document_updates` | Update handling | Newer document versions preserved |
| `test_compaction_idempotent` | Idempotent compaction | Multiple compactions produce same result |

### Integration Tests (in `src/lib.rs`)

| Test | Purpose |
|------|---------|
| `test_collection_delete` | Soft delete via API |

## Phase 6: Hardening Tests

### Property-Based Tests (`proptest`)

Added property-based tests to verify key invariants:

#### WAL Properties (`src/wal.rs`)

| Test | Property Verified |
|------|-------------------|
| `prop_wal_replay_idempotent` | Replaying WAL produces identical results |
| `prop_wal_sequence_monotonic` | Sequence numbers are strictly increasing |
| `prop_record_count_matches_appends` | Record count equals successful appends |
| `prop_delete_records_preserved` | Delete records survive WAL round-trip |

#### Filter Properties (`src/filter.rs`)

| Test | Property Verified |
|------|-------------------|
| `prop_eq_reflexive` | Eq filter matches identical values |
| `prop_eq_deterministic` | Filter evaluation is deterministic |
| `prop_gt_lt_exclusive` | Gt and Lt are mutually exclusive |
| `prop_gte_equivalent` | Gte ≡ Gt OR Eq |
| `prop_lte_equivalent` | Lte ≡ Lt OR Eq |
| `prop_and_empty_always_true` | Empty And returns true |
| `prop_or_empty_always_false` | Empty Or returns false |
| `prop_numeric_coercion_eq` | Integers equal their float equivalents |

#### Distance Properties (`src/distance.rs`)

| Test | Property Verified |
|------|-------------------|
| `prop_dot_product_simd_scalar_match` | SIMD == Scalar (dot product) |
| `prop_cosine_similarity_simd_scalar_match` | SIMD == Scalar (cosine) |
| `prop_euclidean_distance_simd_scalar_match` | SIMD == Scalar (euclidean) |
| `prop_cosine_bounded` | Cosine ∈ [-1, 1] |
| `prop_cosine_symmetric` | cos(a,b) = cos(b,a) |
| `prop_euclidean_symmetric` | dist(a,b) = dist(b,a) |
| `prop_euclidean_non_negative` | Distance ≥ 0 |
| `prop_distance_enum_consistent` | Distance::compute matches functions |

### Benchmarks (`criterion`)

Benchmark suite in `benches/`:

| Benchmark | Variables | Metrics |
|-----------|-----------|---------|
| `bench_search` | Dataset size, ef parameter, dimension | Latency p50/p95/p99 |
| `bench_insert` | Batch size, dimension | Throughput (docs/sec) |
| `bench_recovery` | WAL size, segment count | Recovery time |

Run benchmarks:
```bash
cargo bench
```

---

## Test Count Summary

| Phase | Unit Tests | Integration | Property | Benchmarks | Total |
|-------|------------|-------------|----------|------------|-------|
| Core | 103 | - | - | - | 103 |
| Phase 1A | - | 16 | - | - | 16 |
| Phase 2 | - | 11 | - | - | 11 |
| Phase 3 | - | 7 | - | - | 7 |
| Phase 4 | - | 13 | - | - | 13 |
| Phase 5 | - | 12 | - | - | 12 |
| Phase 6 | - | - | 20 | 3 suites | 20+ |
| **Total** | **103** | **59** | **20** | **3** | **185+** |

---

## Document History

| Date | Change |
|------|--------|
| 2026-02-14 | Initial test documentation for Phase 1A |
| 2026-02-14 | Updated for Phase 2: Search tests, SIMD tests, 70 total tests |
| 2026-02-14 | Updated for Phase 3: HNSW tests, 84 total tests |
| 2026-02-14 | Updated for Phase 4: Filter DSL tests, 140 total tests |
| 2026-02-14 | Updated for Phase 5: Compaction tests, 157 total tests |
| 2026-02-14 | Updated for Phase 6: Property tests, benchmarks, 185+ total tests |

---

## Notes

- **File metadata vs explicit dates:** This file includes explicit timestamps because:
  1. Git history may not be available in all contexts (e.g., source tarballs)
  2. File modification times vary by filesystem and copy operations
  3. Explicit dates make the document self-contained

- **Test determinism:** All tests use fixed seeds or sequential values where applicable. No random data without explicit seeding.

- **Platform coverage:** Currently tested on Windows 11. Unix (Linux/macOS) coverage planned for CI/CD.
