# nDB User Guide

> **Version:** 0.1.0  
> **Last Updated:** 2026-02-14

---

## Table of Contents

1. [Introduction](#introduction)
2. [Quick Start](#quick-start)
3. [Core Concepts](#core-concepts)
4. [API Reference](#api-reference)
5. [Durability Modes](#durability-modes)
6. [Performance Tuning](#performance-tuning)
7. [Best Practices](#best-practices)
8. [Troubleshooting](#troubleshooting)

---

## Introduction

nDB is a high-performance, embedded, in-memory vector database designed for LLM workflows. It provides:

- **Deterministic correctness**: Design failures away rather than handling them
- **Zero-copy reads**: Memory-mapped persistence for instant recovery
- **SIMD-accelerated search**: AVX2/AVX-512/NEON support via the `wide` crate
- **HNSW approximate search**: Configurable recall vs latency tradeoffs
- **MongoDB-compatible filters**: Query by metadata with a familiar DSL

### When to Use nDB

- Storing embeddings from LLMs (768-1536 dimensions)
- Similarity search with optional metadata filtering
- Read-heavy workloads with occasional bulk writes
- Single-node deployments where embedded databases are appropriate

### When NOT to Use nDB

- Distributed/multi-node requirements
- Complex multi-document transactions
- SQL-style queries with joins
- Write-heavy workloads with high concurrency

---

## Quick Start

### Installation

Add nDB to your `Cargo.toml`:

```toml
[dependencies]
ndb = "0.1"
```

### Basic Usage

```rust
use ndb::{Database, CollectionConfig, Document, Distance, Search};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open or create a database
    let db = Database::open("./data")?;
    
    // Create a collection with 768-dimensional vectors
    let collection = db.create_collection(
        "embeddings",
        CollectionConfig::new(768)
    )?;
    
    // Insert a document
    collection.insert(Document {
        id: "doc1".to_string(),
        vector: vec![0.1; 768],  // Your embedding here
        payload: Some(serde_json::json!({
            "title": "Example Document",
            "category": "tutorial"
        })),
    })?;
    
    // Search for similar vectors
    let query = vec![0.1; 768];
    let results = collection.search(
        Search::new(&query)
            .top_k(10)
            .approximate(true)
            .ef(64)
    )?;
    
    for m in results {
        println!("{}: score={}", m.id, m.score);
    }
    
    Ok(())
}
```

---

## Core Concepts

### Collections

A collection is a named group of documents with the same vector dimension. Each collection has:

- Fixed dimension (set at creation)
- Optional HNSW index for approximate search
- Its own WAL and segment files

```rust
// Create collection
let config = CollectionConfig::new(768)
    .with_durability(Durability::FdatasyncEachBatch);
let collection = db.create_collection("my_collection", config)?;

// Get existing collection
let collection = db.get_collection("my_collection")?;
```

### Documents

A document consists of:
- **id**: Unique string identifier
- **vector**: Fixed-dimension f32 array
- **payload**: Optional JSON metadata

```rust
let doc = Document {
    id: "unique_id".to_string(),
    vector: vec![0.1, 0.2, 0.3, /* ... */],
    payload: Some(serde_json::json!({
        "title": "Document Title",
        "tags": ["rust", "vector"],
        "views": 100
    })),
};
```

### Distance Metrics

nDB supports three distance metrics:

| Metric | Range | Higher is Better | Best For |
|--------|-------|------------------|----------|
| `DotProduct` | (-∞, ∞) | Yes | Normalized embeddings |
| `Cosine` | [-1, 1] | Yes | General similarity |
| `Euclidean` | [0, ∞) | No | L2 distance |

```rust
use ndb::{Search, Distance};

// Use cosine similarity (default for HNSW)
let search = Search::new(&query)
    .top_k(10)
    .distance(Distance::Cosine);
```

---

## API Reference

### Database Operations

```rust
// Open or create database
let db = Database::open("./data")?;

// List collections
let names = db.collection_names()?;

// Check if collection exists
if db.has_collection("my_collection")? {
    // ...
}
```

### Document Operations

```rust
// Insert single document
collection.insert(doc)?;

// Batch insert (more efficient)
collection.insert_batch(vec![doc1, doc2, doc3])?;

// Retrieve by ID
if let Some(doc) = collection.get("doc1")? {
    println!("Found: {}", doc.id);
}

// Delete (soft delete, removed on compaction)
collection.delete("doc1")?;

// Flush memtable to segment
collection.flush()?;

// Sync WAL to disk
collection.sync()?;
```

### Search

```rust
use ndb::{Search, Filter};

// Basic exact search
let results = collection.search(
    Search::new(&query_vector)
        .top_k(10)
)?;

// Approximate search with HNSW
let results = collection.search(
    Search::new(&query_vector)
        .top_k(10)
        .approximate(true)
        .ef(128)  // Higher = better recall, slower
)?;

// Search with filter
let results = collection.search(
    Search::new(&query_vector)
        .top_k(10)
        .filter(Filter::eq("category", "tutorial"))
)?;

// Complex filter
let filter = Filter::and(vec![
    Filter::eq("category", "tutorial"),
    Filter::gt("views", 100),
    Filter::or(vec![
        Filter::eq("published", true),
        Filter::eq("author", "admin"),
    ]),
]);
```

### Index Management

```rust
// Build HNSW index
collection.rebuild_index()?;

// Check if index exists
if collection.has_index() {
    // ...
}

// Delete index
collection.delete_index()?;
```

### Compaction

```rust
// Compact collection (remove deleted docs, rebuild index)
let result = collection.compact()?;
println!(
    "Reduced from {} to {} documents",
    result.docs_before,
    result.docs_after
);
```

---

## Durability Modes

nDB offers two durability modes:

### Buffered (Default)

```rust
let config = CollectionConfig::new(768);
// or explicitly:
let config = CollectionConfig::new(768)
    .with_durability(Durability::Buffered);
```

- **Speed**: Fastest (~100K+ inserts/sec)
- **Safety**: Data loss window of ~5-30 seconds (OS dependent)
- **Use case**: High-throughput ingestion where recent data loss is acceptable

### FdatasyncEachBatch

```rust
let config = CollectionConfig::new(768)
    .with_durability(Durability::FdatasyncEachBatch);
```

- **Speed**: Slower (~10K inserts/sec)
- **Safety**: No data loss on crash
- **Use case**: Critical data requiring durability

---

## Performance Tuning

### Search Performance

| Dataset Size | Exact Search | Approximate (HNSW) |
|--------------|--------------|-------------------|
| 10K docs | <1ms | <1ms |
| 100K docs | 5-10ms | 1-2ms |
| 1M docs | 50-100ms | 2-5ms |
| 10M docs | 500ms-1s | 5-10ms |

### HNSW Parameters

```rust
use ndb::{HnswParams, CollectionConfig};

// Custom HNSW parameters
let params = HnswParams::default()
    .with_m(32)      // Higher = better recall, more memory
    .with_ef(200);   // Build-time ef (higher = better index)

// Configure collection with custom params
let config = CollectionConfig::new(768);
// Note: HnswParams are used during index building
```

| Parameter | Default | Effect |
|-----------|---------|--------|
| `M` | 16 | Max connections per layer (2-100) |
| `ef` | 32 | Search scope during build (10-500) |

### Batch Operations

```rust
// Batch insert is much faster than individual inserts
let docs: Vec<Document> = /* ... */;
collection.insert_batch(docs)?;  // Single WAL entry
```

### Compaction Strategy

```rust
// Compact when delete ratio exceeds threshold
let stats = collection.stats()?;
if stats.deleted_count as f32 / stats.total_count as f32 > 0.2 {
    collection.compact()?;
}
```

---

## Best Practices

### 1. Choose the Right Dimension

- Use the dimension of your embedding model
- Common: 384 (small), 768 (medium), 1536 (large)
- Dimension cannot be changed after collection creation

### 2. Use Batch Inserts

```rust
// Good: Batch insert
let docs: Vec<Document> = load_documents();
collection.insert_batch(docs)?;

// Bad: Individual inserts
for doc in docs {
    collection.insert(doc)?;  // Slower, more WAL entries
}
```

### 3. Build HNSW Index for Large Collections

```rust
// Build index after initial data load
for batch in document_batches {
    collection.insert_batch(batch)?;
}
collection.rebuild_index()?;
```

### 4. Handle Updates Correctly

```rust
// Updates are "insert with same ID" - old version deleted on compaction
collection.insert(Document {
    id: "existing_id".to_string(),  // Replaces existing
    vector: new_vector,
    payload: new_payload,
})?;

// Compact periodically to reclaim space
collection.compact()?;
```

### 5. Use Filters Selectively

Filters are applied **after** vector search (post-filtering):

```rust
// If filter is very selective, request more results
let results = collection.search(
    Search::new(&query)
        .top_k(100)  // Request more
        .filter(Filter::eq("rare_field", "rare_value"))
)?;
// May get <100 results due to filtering
```

### 6. Choose Appropriate Durability

```rust
// For bulk imports: use Buffered
let import_config = CollectionConfig::new(768);

// For production serving: use FdatasyncEachBatch
let prod_config = CollectionConfig::new(768)
    .with_durability(Durability::FdatasyncEachBatch);
```

---

## Troubleshooting

### CollectionLocked Error

```
Error: CollectionLocked { name: "my_collection" }
```

**Cause**: Another process has the collection open for writing.

**Solution**: 
- Ensure only one writer per collection
- Drop collection references before reopening

### WrongDimension Error

```
Error: WrongDimension { expected: 768, got: 512 }
```

**Cause**: Vector dimension doesn't match collection config.

**Solution**: Check your embedding model's output dimension.

### Low Recall with HNSW

**Symptoms**: Approximate search returns different results than exact search.

**Solutions**:
1. Increase `ef` parameter: `.ef(128)` or higher
2. Rebuild index with higher `M`: `HnswParams::default().with_m(32)`
3. Use exact search for critical queries: `.approximate(false)`

### Slow Recovery

**Symptoms**: Database takes long time to open.

**Cause**: Large WAL with many records.

**Solution**: Call `flush()` periodically during bulk imports.

### High Memory Usage

**Causes & Solutions**:

| Cause | Solution |
|-------|----------|
| Large HNSW index | Reduce `M` parameter |
| Many segments | Call `compact()` |
| Large payloads | Store minimal metadata |

---

## Migration Guide

### Switching Embedding Models

When changing embedding models (different dimensions):

```rust
// Create new collection with new dimension
let new_coll = db.create_collection(
    "embeddings_v2",
    CollectionConfig::new(1536)  // New dimension
)?;

// Re-embed and migrate documents
for doc in old_coll.iter() {
    let new_embedding = new_model.embed(&doc.text);
    new_coll.insert(Document {
        id: doc.id,
        vector: new_embedding,
        payload: doc.payload,
    })?;
}

// Keep both collections during transition
```

---

## Additional Resources

- **API Documentation**: `cargo doc --open`
- **Benchmarks**: `cargo bench`
- **Repository**: https://github.com/ndb/ndb

---

*For issues and feature requests, please use the GitHub issue tracker.*
