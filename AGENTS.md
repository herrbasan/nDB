# nDB Project - Agent Guide

## Project Overview

nDB is a high-performance, embedded, in-memory vector database designed for LLM workflows. It is written in Rust and provides a minimal but complete API for vector storage and similarity search.

### Key Characteristics

| Attribute | Decision | Rationale |
|-----------|----------|-----------|
| **Language** | Rust (Edition 2021, MSRV 1.75) | Memory safety, SIMD, no GC pauses |
| **Deployment** | Embedded library | Single-node, application-integrated |
| **Storage** | LSM-Lite (memtable + mmap segments) + WAL | Instant writes, zero-copy reads, durability |
| **Serialization** | `rkyv` | True zero-copy deserialization |
| **Validation** | Checksum-once, trust-after | BLAKE3/CRC64 in header; validate on open only |
| **Index** | HNSW (configurable) | Industry-standard ANN, tunable recall |
| **SIMD** | `wide` crate | Stable Rust, portable (AVX2/AVX-512/NEON) |
| **API** | Mongo-like, minimal | Familiar but reduced surface area |
| **Durability** | Configurable | `Buffered` vs `FdatasyncEachBatch` |
| **Safety** | Multi-process locks | `flock` prevents dual-writer corruption |

### Project Structure

```
.
├── Cargo.toml              # Package manifest
├── Cargo.lock              # Dependency lock file
├── src/
│   ├── lib.rs              # Main library, Database and Collection types
│   ├── error.rs            # Error types and Result alias
│   ├── distance.rs         # SIMD distance functions (dot, cosine, euclidean)
│   ├── search.rs           # Search API and exact search implementation
│   ├── filter.rs           # Mongo-like Filter DSL
│   ├── hnsw.rs             # HNSW index for approximate search
│   ├── segment.rs          # Memory-mapped segment files
│   ├── memtable.rs         # In-memory write buffer (HashMap + SoA)
│   ├── wal.rs              # Write-ahead log for crash recovery
│   ├── manifest.rs         # Collection manifest (segments, WAL seq)
│   ├── compaction.rs       # Segment compaction and index rebuild
│   ├── lock.rs             # Multi-process locking (flock)
│   └── id.rs               # Internal/external ID mapping
├── tests/                  # Integration tests by phase
│   ├── phase1a_segment_tests.rs
│   ├── phase1a_locking_tests.rs
│   ├── phase2_search_tests.rs
│   ├── phase3_hnsw_tests.rs
│   ├── phase4_filter_tests.rs
│   └── phase5_compaction_tests.rs
├── benches/                # Criterion benchmarks
│   ├── bench_search.rs
│   ├── bench_insert.rs
│   └── bench_recovery.rs
├── docs/                   # Documentation
│   ├── README.md           # Documentation index
│   ├── development-plan.md # Development phases and roadmap
│   ├── test-documentation.md # Test suite documentation
│   ├── user-guide.md       # User-facing API guide
│   └── handover-*.md       # Phase completion notes
└── examples/               # Usage examples
```

### Core Dependencies

```toml
memmap2 = "0.9"          # Memory-mapped files
rkyv = "0.7"             # Zero-copy serialization
serde = "1.0"            # Serialization framework
serde_json = "1.0"       # JSON support
arc-swap = "1.6"         # Atomic data structure updates
wide = "0.7"             # Portable SIMD
thiserror = "1.0"        # Error derive macros
crc32fast = "1.3"        # CRC32 checksums
parking_lot = "0.12"     # Synchronization primitives
blake3 = "1.5"           # Cryptographic hashing
byteorder = "1.5"        # Byte order handling
bincode = "1.3"          # Binary serialization
fastrand = "2.0"         # Fast random number generation
```

---

## Build and Test Commands

### Building

```bash
# Build debug version
cargo build

# Build release version (optimized)
cargo build --release

# Build and open documentation
cargo doc --open
```

### Testing

```bash
# Run all tests (185+ tests)
cargo test

# Run specific test suite
cargo test --test phase1a_segment_tests
cargo test --test phase1a_locking_tests
cargo test --test phase2_search_tests
cargo test --test phase3_hnsw_tests
cargo test --test phase4_filter_tests
cargo test --test phase5_compaction_tests

# Run with output visible
cargo test -- --nocapture

# Run in release mode (for performance validation)
cargo test --release

# Run property-based tests only
cargo test proptest
```

### Benchmarking

```bash
# Run all benchmarks
cargo bench

# Run specific benchmark
cargo bench --bench bench_search
cargo bench --bench bench_insert
cargo bench --bench bench_recovery
```

### Running Examples

```bash
# Basic usage example
cargo run --example basic_usage

# RAG system example
cargo run --example rag_system

# Web service example
cargo run --example web_service
```

---

## Code Style Guidelines

### Rust Style

- **Edition**: 2021
- **MSRV**: 1.75
- Use `cargo fmt` for formatting
- Use `cargo clippy` for linting (warnings enabled for missing docs and rust_2018_idioms)

### Documentation

- All public items must have doc comments (`#![warn(missing_docs)]`)
- Use `//!` for module-level documentation
- Include examples in doc comments where appropriate
- Mark unsafe code with `SAFETY:` comments

### Error Handling

- Use `thiserror` for error enums
- All errors are explicit (`Result<T, Error>`)
- No panics in library code
- Provide context for I/O errors using `Error::io_err()`

```rust
// Good: Explicit error with context
pub fn io_err(path: impl Into<PathBuf>, context: impl Into<String>) -> impl FnOnce(std::io::Error) -> Self

// Good: Specific error variants
#[error("collection '{name}' is locked by another process")]
CollectionLocked { name: String }
```

### Naming Conventions

- Types: `PascalCase`
- Functions/variables: `snake_case`
- Constants: `SCREAMING_SNAKE_CASE`
- Modules: `snake_case`

### Module Organization

```rust
// lib.rs pattern: re-export public API
pub mod error;
pub mod distance;
// ...

// Re-export commonly used items
pub use error::{Error, Result};
pub use distance::Distance;
// ...
```

---

## Testing Instructions

### Test Organization

| Location | Purpose |
|----------|---------|
| `src/*/tests` | Unit tests within modules (using `#[cfg(test)]`) |
| `tests/` | Integration tests by development phase |
| `benches/` | Criterion benchmarks |

### Writing Unit Tests

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_feature_description() {
        // Arrange
        let input = ...;
        
        // Act
        let result = function_under_test(input);
        
        // Assert
        assert_eq!(result, expected);
    }

    // Use proptest for property-based testing
    use proptest::prelude::*;
    proptest! {
        #[test]
        fn prop_invariant_holds(input in strategy()) {
            prop_assert!(check_invariant(input));
        }
    }
}
```

### Writing Integration Tests

```rust
// tests/phaseX_feature_tests.rs
use ndb::{Database, CollectionConfig, Document, /* ... */};
use tempfile::TempDir;

#[test]
fn test_scenario_description() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();
    
    // Test code...
}
```

### Test Data Guidelines

- Use `tempfile::TempDir` for test isolation
- Use sequential values (0.0, 1.0, 2.0...) for deterministic vectors
- Test dimensions: 4 (unit tests), 384, 768, 1536 (integration)
- Clean up is automatic via `TempDir` drop

### Property-Based Testing

Use `proptest` for verifying invariants:

```rust
proptest! {
    #[test]
    fn prop_simd_scalar_match(
        a in prop::collection::vec(-10.0f32..10.0, 1..100),
        b in prop::collection::vec(-10.0f32..10.0, 1..100)
    ) {
        let simd = simd_function(&a, &b);
        let scalar = scalar_function(&a, &b);
        prop_assert!((simd - scalar).abs() < 1e-3);
    }
}
```

---

## Security Considerations

### Input Validation

- Vector dimensions are validated on insert
- Document IDs must be valid strings
- JSON payloads are validated during deserialization

```rust
// Dimension validation
if doc.vector.len() != self.config.dim {
    return Err(Error::WrongDimension {
        expected: self.config.dim,
        got: doc.vector.len(),
    });
}
```

### File Security

- Checksum verification on segment open (BLAKE3)
- CRC32 on WAL records
- Atomic file updates (write-to-temp + rename)
- `flock` for single-writer enforcement across processes

### Memory Safety

- All `unsafe` code must have `// SAFETY:` comments
- Prefer safe Rust; unsafe only for SIMD or mmap access
- mmap is read-only; no mutable aliasing

### Potential Risks

| Risk | Mitigation |
|------|------------|
| SIGBUS on storage error | File size check before mmap; `madvise(MADV_POPULATE_READ)` |
| Data corruption | Checksum verification; WAL for recovery |
| Concurrent write | `flock` exclusive lock |
| Unbounded memory | WAL flush threshold (64MB); compaction |

### Unsafe Code Guidelines

When using `unsafe`:

```rust
// SAFETY: Vector is 64-byte aligned, dimension matches
unsafe {
    let ptr = vector_data.as_ptr() as *const f32x8;
    // ...
}
```

---

## Architecture Overview

### Storage Model (LSM-Lite)

| Component | Purpose | Mutability |
|-----------|---------|------------|
| **Memtable** | Recent writes in RAM | Mutable (WAL-backed) |
| **Segments** | Immutable historical data | Read-only, mmap'd |
| **WAL** | Durability log | Append-only |
| **HNSW** | ANN index | Rebuilt on compaction |
| **Manifest** | Atomic state transitions | Atomic rename only |

### Write Path

1. Append to WAL (fdatasync if `Durability::FdatasyncEachBatch`)
2. Insert into memtable (`HashMap` + SoA buffer)
3. Return immediately
4. When WAL exceeds threshold (64MB) or `flush()` called:
   - Freeze memtable → write to new segment
   - Update manifest → reset WAL

### Read Path

1. Search memtable (HashMap lookup or SoA scan)
2. Search all segments (mmap'd, zero-copy)
3. Merge results

### Concurrency Model

| Component | Access Pattern | Implementation |
|-----------|---------------|----------------|
| Segments | Immutable, lock-free | mmap + Arc |
| Segment list | Atomic updates | `ArcSwap<Vec<Arc<Segment>>>` |
| Memtable | Read-heavy, write-exclusive | `RwLock<HashMap>` + SoA buffer |
| WAL | Append-only, single writer | `Mutex<File>` |
| Manifest | Atomic replace only | Write-temp + rename |

**Invariants:**
- Single writer per collection (enforced by `flock`)
- Readers never block (immutable segments)
- Writers don't block readers (copy memtable, swap Arc)

---

## API Quick Reference

### Database Operations

```rust
let db = Database::open(path)?;              // Open or create
let coll = db.create_collection(name, config)?;  // New collection
let coll = db.get_collection(name)?;             // Existing collection
let names = db.list_collections();               // List collections
```

### Document Operations

```rust
coll.insert(document)?;                      // Add or replace by ID
coll.insert_batch(documents)?;               // Bulk insert
coll.get(id)?;                               // Retrieve by ID
coll.delete(id)?;                            // Soft delete
```

### Search

```rust
use ndb::{Search, Distance, Filter};

// Exact search
let results = coll.search(
    Search::new(&query).top_k(10).distance(Distance::Cosine)
)?;

// Approximate search with HNSW
let results = coll.search(
    Search::new(&query).top_k(10).approximate(true).ef(100)
)?;

// With filter
let results = coll.search(
    Search::new(&query)
        .top_k(10)
        .filter(Filter::and([
            Filter::eq("category", "books"),
            Filter::gt("year", 2020),
        ]))
)?;
```

### Maintenance

```rust
coll.flush()?;                               // Freeze memtable → segment
coll.compact()?;                             // Remove deleted docs, rebuild index
coll.sync()?;                                // Ensure WAL durability
coll.rebuild_index()?;                       // Build HNSW index
coll.delete_index()?;                        // Remove HNSW index
```

---

## Documentation Maintenance

**When modifying code, you MUST update corresponding documentation:**

1. `docs/development-plan.md` - Update when changing phase deliverables, success criteria, or technical decisions. Add entries to the Decision Log.

2. `docs/test-documentation.md` - Update when:
   - Adding, removing, or modifying tests
   - Changing test data characteristics
   - Updating success criteria verification
   - Adding new test suites or phases

3. `AGENTS.md` (this file) - Update when:
   - Changing architectural decisions
   - Modifying file formats or protocols
   - Adding new constraints or non-goals
   - Changing build/test commands

**Documentation that does not match code is a bug.**

---

## Non-Goals

- **Embedding generation** (see `docs/scope-and-boundaries.md`)
- Distributed operation (use application-layer sharding if needed)
- ACID transactions across multiple documents
- Complex query language (no joins, aggregations)
- Network protocol (embedded only)
- Schema migrations within a collection (fixed dimension at creation)
- Background/async compaction for v1.0 (synchronous only)
- Multi-writer per collection (single writer enforced)

---

*See `docs/development-plan.md` for detailed development phases.*
*See `docs/test-documentation.md` for test suite details.*
*See `docs/user-guide.md` for API usage examples.*
