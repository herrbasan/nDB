# nDB Documentation

Welcome to the nDB documentation. This directory contains comprehensive guides for using and integrating nDB.

## Quick Start

New to nDB? Start here:

1. **[User Guide](user-guide.md)** - Complete API reference and usage examples
2. **[Integration Guide](integration-guide.md)** - Production integration patterns
3. **[Integrations](./integrations/)** - Language/environment specific guides
   - [N-API (Node.js native)](./integrations/napi.md) - Maximum performance for Node.js
   - [WebAssembly](./integrations/wasm.md) - Browser and Edge compatibility
   - [gRPC](./integrations/grpc.md) - Multi-language and distributed systems
4. **[Examples](../examples/)** - Working code examples

## Documentation Index

### User Documentation

| Document | Description |
|----------|-------------|
| [User Guide](user-guide.md) | Complete API reference, configuration, best practices |
| [Integration Guide](integration-guide.md) | Integration patterns, platform setup, deployment |
| [Integrations](./integrations/) | Language-specific integration guides (Node.js, WASM, gRPC) |
| [Test Documentation](test-documentation.md) | Test suite documentation |

### Architecture & Design

| Document | Description |
|----------|-------------|
| [Scope and Boundaries](scope-and-boundaries.md) | What nDB is and isn't (no embedding generation) |
| [ADR 001: LSM-Lite Storage](adr/001-lsm-lite-storage.md) | Storage architecture decisions |
| [ADR 002: HNSW Index](adr/002-hnsw-index.md) | Approximate search design |
| [ADR 003: SIMD Distance](adr/003-simd-distance.md) | SIMD computation choices |

### Development

| Document | Description |
|----------|-------------|
| [Development Plan](development-plan.md) | Development phases and roadmap |
| [Handover Phase 5→6](handover-phase5-to-phase6.md) | Phase 5 completion notes |

## Common Tasks

### Adding nDB to Your Project

```toml
[dependencies]
ndb = "0.1"
serde_json = "1.0"
```

```rust
use ndb::{Database, CollectionConfig, Document, Search};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::open("./data")?;
    let collection = db.create_collection("embeddings", CollectionConfig::new(768))?;
    
    // Insert documents, search, etc.
    
    Ok(())
}
```

### Running Examples

```bash
# Basic usage example
cargo run --example basic_usage

# RAG system example
cargo run --example rag_system

# Web service example (requires additional dependencies)
cargo run --example web_service
```

### Running Tests

```bash
# All tests
cargo test

# Specific test suite
cargo test --test phase5_compaction_tests

# With output
cargo test -- --nocapture
```

### Running Benchmarks

```bash
# All benchmarks
cargo bench

# Specific benchmark
cargo bench --bench bench_search
```

## Key Concepts

### Collections

Collections are named groups of documents with the same vector dimension. Think of them like tables in SQL databases.

```rust
let config = CollectionConfig::new(768);  // 768-dimensional vectors
let collection = db.create_collection("embeddings", config)?;
```

### Documents

Documents contain:
- **id**: Unique string identifier
- **vector**: The embedding vector
- **payload**: Optional JSON metadata

### Search Types

| Type | Speed | Recall | Use Case |
|------|-------|--------|----------|
| Exact | Slower | 100% | Small datasets (<100K) |
| Approximate (HNSW) | Faster | 90-99% | Large datasets (>100K) |

### Durability Modes

| Mode | Speed | Safety | Use Case |
|------|-------|--------|----------|
| Buffered | Fastest | 5-30s window | Bulk imports |
| FdatasyncEachBatch | Slower | Guaranteed | Production serving |

## Support

- **GitHub Issues**: https://github.com/ndb/ndb/issues
- **API Docs**: Run `cargo doc --open`
- **Examples**: See `examples/` directory

## License

MIT OR Apache-2.0
