# Handover: Phase 3 → Phase 4

**Date:** 2026-02-14  
**Status:** Phase 3 COMPLETE → Phase 4 READY  
**Tests:** 84 passing

---

## Phase 3 Summary (What Was Built)

### New Modules

| Module | Purpose | Key Types/Functions |
|--------|---------|---------------------|
| `src/hnsw.rs` | HNSW index implementation | `HnswIndex`, `HnswBuilder`, `HnswParams` |
| `tests/phase3_hnsw_tests.rs` | Integration tests | 7 tests for HNSW API |

### Modified Files

| File | Changes |
|------|---------|
| `src/search.rs` | Added `approximate: bool`, `ef: Option<usize>` to `Search` builder |
| `src/manifest.rs` | Added `index_file`, `index_generation` fields |
| `src/lib.rs` | Added `hnsw_index`, `rebuild_index()`, `delete_index()`, `has_index()` |
| `src/distance.rs` | Added `serde::Serialize/Deserialize` to `Distance` enum |
| `Cargo.toml` | Added `bincode`, `fastrand` dependencies |

### API

```rust
// Approximate search (NEW)
let results = coll.search(
    &Search::new(&query_vec)
        .top_k(10)
        .distance(Distance::Cosine)
        .approximate(true)   // NEW: use HNSW
        .ef(100)             // NEW: search-time quality parameter
)?;

// Explicit index management (NEW)
coll.rebuild_index()?;     // Build HNSW from all segments
coll.delete_index()?;      // Remove index (fallback to exact)
coll.has_index();          // Check if index exists
```

### Performance Baseline (Approximate Search)

| Dataset | Dim | Count | Notes |
|---------|-----|-------|-------|
| Recall | 768 | 100K | >30% on synthetic data (basic implementation) |
| Build time | - | 500 | <100ms for 500 vectors |
| Memory | - | - | CSR layout ~20-40% smaller than pointer-based |

**Key Point:** Approximate search is functional. Recall can be improved with parameter tuning.

---

## Phase 4: Query Interface — Mongo-Like Filter DSL (Next Session)

### Goal
Implement an ergonomic, type-safe Filter DSL for metadata filtering with MongoDB-like syntax.

### Deliverables

1. **Filter DSL Types** (`src/filter.rs`)
   - `Filter` enum with variants: `Eq`, `Gt`, `Gte`, `Lt`, `Lte`, `And`, `Or`, `In`
   - Builder pattern for ergonomic construction
   - Type-safe operations (compile-time validation where possible)

2. **Search Integration**
   - `Search::filter(Filter)` builder method
   - Post-filtering: Search vectors, then filter results
   - Pre-filtering consideration: HNSW candidate filtering (optional for v1)

3. **Filter Execution Engine**
   - Evaluate `Filter` against `Document` payload
   - JSON value traversal (dot notation for nested fields: `"user.name"`)
   - Type coercion (e.g., compare int to float)

4. **JSON Macro Support** (optional)
   - `filter!` macro for JSON-like syntax
   - Example: `filter!({"category": "books", "year": { "$gt": 2020 }})`

### New API

```rust
// Basic filter
let results = coll.search(
    Search::new(&query)
        .top_k(10)
        .filter(Filter::and([
            Filter::eq("category", "books"),
            Filter::gt("year", 2020),
        ]))
)?;

// With approximate search
let results = coll.search(
    Search::new(&query)
        .top_k(10)
        .approximate(true)
        .ef(100)
        .filter(Filter::eq("status", "active"))
)?;
```

### Success Criteria

| Metric | Target | How to Verify |
|--------|--------|---------------|
| Filter correctness | 100% | Unit tests for all predicate types |
| Post-filter recall | Documented | May return <k results if filter is selective |
| Performance | <2x overhead | Filter evaluation <50% of search time |
| Ergonomics | Intuitive | API review, example code readability |

---

## Entry Points for Phase 4

### Files to Modify

1. **`src/search.rs`**
   - Add `filter: Option<Filter>` to `Search`
   - Add builder method: `.filter()`
   - Modify `exact_search()` to apply filter to results

2. **`src/lib.rs`** (Collection)
   - Pass filter to search functions
   - Integration with HNSW search (filter candidates)

### New Files to Create

```
src/
├── filter.rs           # Filter DSL and execution
└── filter/
    ├── dsl.rs          # Filter enum and builder
    └── eval.rs         # Filter evaluation engine
```

### Key Data Structures

```rust
// Filter DSL (src/filter.rs)
pub enum Filter {
    Eq { field: String, value: Value },
    Gt { field: String, value: Value },
    Gte { field: String, value: Value },
    Lt { field: String, value: Value },
    Lte { field: String, value: Value },
    In { field: String, values: Vec<Value> },
    And(Vec<Filter>),
    Or(Vec<Filter>),
}

// Search with filter (src/search.rs)
pub struct Search<'a> {
    vector: &'a [f32],
    top_k: usize,
    distance: Distance,
    approximate: bool,
    ef: Option<usize>,
    // NEW FIELD:
    filter: Option<Filter>,
}
```

---

## Filter Execution Strategy

### Post-Filtering (Phase 4 MVP)

```
1. Search vectors (exact or HNSW)
2. For each candidate:
   a. Load document payload
   b. Evaluate filter against payload
   c. Keep if filter matches
3. Return filtered results (may be < k)
```

**Trade-offs:**
- Simple to implement
- Guaranteed recall (no false negatives from filter)
- May return fewer than k results if filter is selective
- Wasted work: we compute distances for documents that get filtered out

### Pre-Filtering (Future Enhancement)

```
1. Build bitmap of documents matching filter
2. Search only within matching documents
3. Return top-k from filtered set
```

**Trade-offs:**
- More efficient (fewer distance computations)
- Requires index on filter fields
- More complex implementation

**Recommendation:** Start with post-filtering. Add pre-filtering in Phase 5+ if performance requires it.

---

## Testing Strategy for Phase 4

### Unit Tests (src/filter.rs)

| Test | Purpose |
|------|---------|
| `test_filter_eq` | Equality predicate |
| `test_filter_gt` | Greater than |
| `test_filter_and` | Logical AND |
| `test_filter_or` | Logical OR |
| `test_filter_nested` | Dot notation (e.g., "user.name") |
| `test_filter_type_coercion` | Int vs float comparison |

### Integration Tests (tests/phase4_filter_tests.rs)

| Test | Purpose |
|------|---------|
| `test_filter_with_exact_search` | Post-filter with exact search |
| `test_filter_with_hnsw` | Post-filter with approximate search |
| `test_filter_selective` | Filter returns <k results |
| `test_filter_no_matches` | Empty result when no matches |
| `test_filter_complex_nested` | Complex AND/OR combinations |

---

## Open Design Questions

### 1. Field Path Syntax
- **Option A:** Simple string (`"user.name"`)
- **Option B:** Array of path components (`&["user", "name"]`)
- **Recommendation:** A (MongoDB-compatible, intuitive)

### 2. Type Coercion
- **Question:** Should `Filter::gt("count", 5)` match `{"count": 5.5}`?
- **Recommendation:** Yes, coerce numerics. Fail on incompatible types (string vs number).

### 3. Missing Fields
- **Option A:** Missing field = filter fails (document excluded)
- **Option B:** Missing field = null comparison
- **Recommendation:** A (explicit is better than implicit)

### 4. Array Handling
- **Question:** How to filter arrays? (`{"tags": ["a", "b", "c"]}`)
- **Option A:** `$in` operator matches if any element matches
- **Option B:** Separate `$contains` operator
- **Recommendation:** A (MongoDB-compatible)

---

## Entry Points (Code Locations)

### Current Search Flow
```
Collection::search(query)
    └── if query.approximate && hnsw_index.exists():
            search_hnsw(query) → Vec<Match>
        else:
            exact_search(memtable, segments, query) → Vec<Match>
```

### New Search Flow with Filter
```
Collection::search(query)
    └── results = if query.approximate && hnsw_index.exists():
                      search_hnsw(query)
                  else:
                      exact_search(...)
    └── if let Some(filter) = query.filter:
            results.retain(|m| filter.evaluate(m.payload))
    └── return results
```

### Files to Read First

1. `src/search.rs:40-100` - `Search` struct and builder
2. `src/search.rs:178-280` - `exact_search()` implementation
3. `src/segment.rs:600-610` - `get_payload()` method
4. `src/memtable.rs:39-50` - `MemtableDoc` payload field

---

## Quick Start Commands

```bash
# Verify Phase 3 still works
cargo test

# Create Phase 4 test scaffold
touch tests/phase4_filter_tests.rs

# Run just filter tests (once written)
cargo test --test phase4_filter_tests
```

---

## References

- **MongoDB Query Operators:** https://docs.mongodb.com/manual/reference/operator/query/
- **Filter DSL Inspiration:** 
  - MongoDB's query syntax
  - Firebase Firestore queries
  - DynamoDB condition expressions
- **Existing Code:**
  - `src/search.rs` - Search builder pattern
  - `src/distance.rs` - Type-safe enum pattern

---

## Checklist for Phase 4 Completion

- [ ] `src/filter.rs` with `Filter` enum
- [ ] All basic predicates: Eq, Gt, Gte, Lt, Lte, In
- [ ] Logical operators: And, Or
- [ ] Nested field access (dot notation)
- [ ] `Search::filter()` builder method
- [ ] Post-filtering in `exact_search()`
- [ ] Post-filtering in `search_hnsw()`
- [ ] Unit tests for filter evaluation
- [ ] Integration tests with search
- [ ] Documentation update
- [ ] Example code in docs

---

## Notes for Next Session

1. **Start Simple:** Build the `Filter` enum and evaluation first, then integrate with search.

2. **Payload Access:** The filter needs to evaluate against `serde_json::Value`. Use `get()` for dot notation access.

3. **Error Handling:** Consider what happens when filter references non-existent fields. Return error or exclude document?

4. **Performance:** Post-filtering is acceptable for MVP. Document the limitation (may return <k results).

5. **Type Safety:** Rust's type system can help. Consider making `Filter` generic over value types, or stick with `serde_json::Value` for flexibility.

6. **Builder Pattern:** Follow the pattern established in `Search` - chainable methods, consuming builder.

---

*See `docs/development-plan.md` for detailed Phase 4 requirements.*
