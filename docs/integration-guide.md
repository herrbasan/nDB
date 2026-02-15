# nDB Integration Guide

> **Version:** 0.1.0  
> **Last Updated:** 2026-02-14

---

## Table of Contents

1. [Overview](#overview)
2. [Integration Patterns](#integration-patterns)
3. [Platform-Specific Setup](#platform-specific-setup)
4. [Language Bindings](#language-bindings)
5. [Configuration Management](#configuration-management)
6. [Use Case Examples](#use-case-examples)
7. [Deployment Strategies](#deployment-strategies)
8. [Performance Optimization](#performance-optimization)
9. [Monitoring & Observability](#monitoring--observability)
10. [Troubleshooting](#troubleshooting)

---

## Overview

nDB is designed as an embedded vector database for applications requiring high-performance similarity search. This guide covers integration patterns for various architectures and platforms.

### When to Integrate nDB

- **RAG Systems**: Store and retrieve document embeddings for LLM context
- **Recommendation Engines**: Find similar items based on vector similarity
- **Image Search**: Semantic image retrieval via CLIP embeddings
- **Anomaly Detection**: Find outliers in vector space
- **Semantic Cache**: Cache LLM responses by embedding similarity

### Integration Architecture

```
┌─────────────────────────────────────────┐
│           Your Application              │
│  ┌─────────────┐    ┌────────────────┐ │
│  │   Business  │───▶│   nDB Embed    │ │
│  │    Logic    │◀───│   (in-process) │ │
│  └─────────────┘    └────────────────┘ │
└─────────────────────────────────────────┘
              │
              ▼
┌─────────────────────────────────────────┐
│         Local Filesystem                │
│    (WAL, Segments, Index Files)         │
└─────────────────────────────────────────┘
```

---

## Integration Patterns

### Pattern 1: Direct Rust Integration

For Rust applications, nDB is embedded directly as a library.

```rust
// Cargo.toml
[dependencies]
ndb = "0.1"
serde_json = "1.0"

// src/main.rs
use ndb::{Database, CollectionConfig, Document, Search};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Database is embedded in your application
    let db = Database::open("./data")?;
    
    // Use throughout your application lifecycle
    let collection = db.create_collection("embeddings", CollectionConfig::new(768))?;
    
    // Your application logic here
    
    Ok(()) // Database closes cleanly on drop
}
```

### Pattern 2: Embedded in a Service

Wrap nDB in a service layer for multi-threaded access:

```rust
use std::sync::Arc;
use ndb::{Database, Collection};

pub struct VectorService {
    db: Arc<Database>,
}

impl VectorService {
    pub fn new(data_dir: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let db = Arc::new(Database::open(data_dir)?);
        Ok(Self { db })
    }
    
    pub fn search_similar(
        &self,
        collection: &str,
        query: &[f32],
        top_k: usize,
    ) -> Result<Vec<Match>, ndb::Error> {
        let coll = self.db.get_collection(collection)?;
        let search = Search::new(query).top_k(top_k);
        coll.search(&search)
    }
}

// Usage in web framework (e.g., axum/actix)
let service = Arc::new(VectorService::new("./data")?);

// Clone Arc for each handler
async fn search_handler(
    State(service): State<Arc<VectorService>>,
    Json(request): Json<SearchRequest>,
) -> impl IntoResponse {
    let results = service.search_similar(
        &request.collection,
        &request.vector,
        request.top_k,
    )?;
    Json(results)
}
```

### Pattern 3: Background Ingestion Pipeline

For high-throughput scenarios, use a channel-based pipeline:

```rust
use tokio::sync::mpsc;

pub struct IngestionPipeline {
    sender: mpsc::Sender<Document>,
}

impl IngestionPipeline {
    pub fn new(db: Arc<Database>, collection: String) -> Self {
        let (sender, mut receiver) = mpsc::channel::<Document>(10000);
        
        tokio::spawn(async move {
            let coll = db.get_collection(&collection).unwrap();
            let mut batch = Vec::with_capacity(100);
            
            while let Some(doc) = receiver.recv().await {
                batch.push(doc);
                
                if batch.len() >= 100 {
                    coll.insert_batch(std::mem::take(&mut batch)).unwrap();
                }
            }
            
            // Flush remaining
            if !batch.is_empty() {
                coll.insert_batch(batch).unwrap();
            }
        });
        
        Self { sender }
    }
    
    pub async fn ingest(&self, doc: Document) -> Result<(), mpsc::error::SendError<Document>> {
        self.sender.send(doc).await
    }
}
```

### Pattern 4: Read Replicas (Single Writer, Multiple Readers)

nDB supports multiple processes reading while one writes:

```rust
// Writer process
fn writer_process() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::open("./data")?;
    let coll = db.create_collection("vectors", CollectionConfig::new(768))?;
    
    loop {
        // Receive updates from message queue
        let update = receive_from_queue().await?;
        coll.insert(update)?;
    }
}

// Reader processes (multiple instances)
fn reader_process() -> Result<(), Box<dyn std::error::Error>> {
    let db = Database::open("./data")?;
    let coll = db.get_collection("vectors")?;
    
    // Serve search requests
    loop {
        let request = receive_search_request().await?;
        let results = coll.search(&request)?;
        send_response(results).await?;
    }
}
```

**Important**: Only one process can have a collection open for writing at a time.

---

## Platform-Specific Setup

### Linux

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Add to Cargo.toml
[dependencies]
ndb = "0.1"

# Build
 cargo build --release
```

**System Requirements:**
- Kernel 4.14+ for proper mmap support
- glibc 2.28+ (most modern distributions)
- AVX2 support recommended (check with `cat /proc/cpuinfo | grep avx2`)

**Docker Integration:**

```dockerfile
FROM rust:1.75 as builder
WORKDIR /app
COPY . .
RUN cargo build --release

FROM debian:bookworm-slim
RUN apt-get update && apt-get install -y libssl3
COPY --from=builder /app/target/release/myapp /usr/local/bin/
VOLUME ["/data"]
ENV NDB_DATA_DIR=/data
CMD ["myapp"]
```

### macOS

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# For Apple Silicon (M1/M2/M3)
rustup target add aarch64-apple-darwin

# Build native binary
cargo build --release
```

**Universal Binary:**

```bash
# Build for both architectures
cargo build --release --target x86_64-apple-darwin
cargo build --release --target aarch64-apple-darwin

# Create universal binary
lipo -create \
  target/x86_64-apple-darwin/release/myapp \
  target/aarch64-apple-darwin/release/myapp \
  -output target/universal/myapp
```

### Windows

```powershell
# Install Rust via rustup-init.exe
# Download from https://rustup.rs/

# Build
 cargo build --release
```

**Considerations:**
- Use `memmap2` crate (already included) for cross-platform mmap
- Path handling: Use `std::path::PathBuf` for cross-platform paths
- File locking: Uses Windows `LockFileEx` API

---

## Language Bindings

### Python (via PyO3)

Create a Python wrapper using PyO3:

```rust
// src/lib.rs (Python module)
use pyo3::prelude::*;
use ndb::{Database, CollectionConfig, Document, Search};

#[pyclass]
struct PyDatabase {
    db: Database,
}

#[pymethods]
impl PyDatabase {
    #[new]
    fn new(path: &str) -> PyResult<Self> {
        Ok(Self {
            db: Database::open(path).map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?,
        })
    }
    
    fn search(&self, collection: &str, query: Vec<f32>, top_k: usize) -> PyResult<Vec<PyMatch>> {
        let coll = self.db.get_collection(collection)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        
        let search = Search::new(&query).top_k(top_k);
        let results = coll.search(&search)
            .map_err(|e| pyo3::exceptions::PyRuntimeError::new_err(e.to_string()))?;
        
        Ok(results.into_iter().map(|m| PyMatch {
            id: m.id,
            score: m.score,
        }).collect())
    }
}

#[pyclass]
struct PyMatch {
    #[pyo3(get)]
    id: String,
    #[pyo3(get)]
    score: f32,
}

#[pymodule]
fn ndb_py(_py: Python, m: &PyModule) -> PyResult<()> {
    m.add_class::<PyDatabase>()?;
    m.add_class::<PyMatch>()?;
    Ok(())
}
```

```python
# setup.py
from setuptools import setup
from setuptools_rust import Binding, RustExtension

setup(
    name="ndb-py",
    version="0.1.0",
    rust_extensions=[RustExtension("ndb_py", binding=Binding.PyO3)],
    packages=["ndb_py"],
    zip_safe=False,
)
```

```python
# usage.py
import ndb_py

db = ndb_py.PyDatabase("./data")
results = db.search("embeddings", [0.1] * 768, top_k=10)
for r in results:
    print(f"{r.id}: {r.score}")
```

### Node.js (via N-API)

```rust
// Native module using napi-rs
use napi_derive::napi;
use ndb::{Database, Search};

#[napi]
pub struct NDB {
    db: Database,
}

#[napi]
impl NDB {
    #[napi(constructor)]
    pub fn new(path: String) -> napi::Result<Self> {
        Ok(Self {
            db: Database::open(&path).map_err(|e| {
                napi::Error::from_reason(e.to_string())
            })?,
        })
    }
    
    #[napi]
    pub fn search(
        &self,
        collection: String,
        query: Vec<f32>,
        top_k: u32,
    ) -> napi::Result<Vec<Match>> {
        let coll = self.db.get_collection(&collection)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        
        let search = Search::new(&query).top_k(top_k as usize);
        let results = coll.search(&search)
            .map_err(|e| napi::Error::from_reason(e.to_string()))?;
        
        Ok(results.into_iter().map(|m| Match {
            id: m.id,
            score: m.score,
        }).collect())
    }
}

#[napi(object)]
pub struct Match {
    pub id: String,
    pub score: f32,
}
```

```javascript
// usage.js
const { NDB } = require('./index.node');

const db = new NDB('./data');
const results = db.search('embeddings', new Array(768).fill(0.1), 10);
console.log(results);
```

### Go (via CGO)

```go
// ndb.go
package ndb

/*
#cgo LDFLAGS: -L. -lndb_c
#include "ndb_c.h"
*/
import "C"
import (
    "unsafe"
)

type DB struct {
    ptr unsafe.Pointer
}

func Open(path string) (*DB, error) {
    cpath := C.CString(path)
    defer C.free(unsafe.Pointer(cpath))
    
    ptr := C.ndb_open(cpath)
    if ptr == nil {
        return nil, fmt.Errorf("failed to open database")
    }
    
    return &DB{ptr: ptr}, nil
}

func (db *DB) Close() {
    C.ndb_close(db.ptr)
}
```

---

## Configuration Management

### Environment-Based Configuration

```rust
use std::env;

pub struct Config {
    pub data_dir: String,
    pub default_dim: usize,
    pub durability: Durability,
    pub hnsw_m: usize,
    pub hnsw_ef: usize,
}

impl Config {
    pub fn from_env() -> Self {
        Self {
            data_dir: env::var("NDB_DATA_DIR").unwrap_or_else(|_| "./data".to_string()),
            default_dim: env::var("NDB_DEFAULT_DIM")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(768),
            durability: match env::var("NDB_DURABILITY").as_deref() {
                Ok("sync") => Durability::FdatasyncEachBatch,
                _ => Durability::Buffered,
            },
            hnsw_m: env::var("NDB_HNSW_M")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(16),
            hnsw_ef: env::var("NDB_HNSW_EF")
                .ok()
                .and_then(|s| s.parse().ok())
                .unwrap_or(64),
        }
    }
}
```

### YAML Configuration

```yaml
# ndb.yaml
data_dir: ./data
default_dim: 768
durability: buffered  # or "sync"

hnsw:
  m: 16
  ef_construct: 32
  ef_search: 64

collections:
  - name: text_embeddings
    dim: 768
    distance: cosine
    
  - name: image_embeddings
    dim: 512
    distance: dot
```

```rust
use serde::Deserialize;

#[derive(Deserialize)]
pub struct AppConfig {
    pub data_dir: String,
    pub default_dim: usize,
    #[serde(default)]
    pub durability: DurabilityConfig,
    pub hnsw: HnswConfig,
    pub collections: Vec<CollectionConfig>,
}

#[derive(Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum DurabilityConfig {
    Buffered,
    Sync,
}

impl Default for DurabilityConfig {
    fn default() -> Self { Self::Buffered }
}

// Load
let config: AppConfig = serde_yaml::from_str(&std::fs::read_to_string("ndb.yaml")?)?;
```

---

## Use Case Examples

### Use Case 1: RAG (Retrieval-Augmented Generation)

```rust
use ndb::{Database, CollectionConfig, Document, Search, Filter};

pub struct RAGSystem {
    db: Database,
}

impl RAGSystem {
    pub fn new(data_dir: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let db = Database::open(data_dir)?;
        
        // Create collection if not exists
        if !db.has_collection("documents")? {
            db.create_collection(
                "documents",
                CollectionConfig::new(1536)  // OpenAI embedding dimension
                    .with_durability(Durability::FdatasyncEachBatch),
            )?;
        }
        
        Ok(Self { db })
    }
    
    pub fn index_document(
        &self,
        doc_id: &str,
        text: &str,
        metadata: serde_json::Value,
    ) -> Result<(), ndb::Error> {
        // Generate embedding (using your preferred model)
        let embedding = generate_embedding(text);
        
        let coll = self.db.get_collection("documents")?;
        coll.insert(Document {
            id: doc_id.to_string(),
            vector: embedding,
            payload: Some(metadata),
        })?;
        
        Ok(())
    }
    
    pub fn retrieve_context(
        &self,
        query: &str,
        top_k: usize,
        source_filter: Option<&str>,
    ) -> Result<Vec<ContextChunk>, ndb::Error> {
        let query_embedding = generate_embedding(query);
        
        let coll = self.db.get_collection("documents")?;
        
        let mut search = Search::new(&query_embedding)
            .top_k(top_k)
            .approximate(true)
            .ef(128);
        
        // Apply source filter if provided
        if let Some(source) = source_filter {
            search = search.filter(Filter::eq("source", source));
        }
        
        let results = coll.search(&search)?;
        
        Ok(results.into_iter().map(|m| ContextChunk {
            doc_id: m.id,
            score: m.score,
            metadata: m.payload,
        }).collect())
    }
}
```

### Use Case 2: Semantic Image Search

```rust
pub struct ImageSearchEngine {
    db: Database,
}

impl ImageSearchEngine {
    pub fn new(data_dir: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let db = Database::open(data_dir)?;
        
        // CLIP embeddings are 512-dimensional
        if !db.has_collection("images")? {
            db.create_collection(
                "images",
                CollectionConfig::new(512),
            )?;
        }
        
        Ok(Self { db })
    }
    
    pub fn index_image(
        &self,
        image_path: &str,
        tags: Vec<String>,
    ) -> Result<(), Box<dyn std::error::Error>> {
        // Extract CLIP embedding
        let embedding = clip_embed_image(image_path);
        
        let coll = self.db.get_collection("images")?;
        coll.insert(Document {
            id: image_path.to_string(),
            vector: embedding,
            payload: Some(serde_json::json!({
                "path": image_path,
                "tags": tags,
            })),
        })?;
        
        Ok(())
    }
    
    pub fn search_by_text(
        &self,
        text_query: &str,
        tag_filter: Option<String>,
    ) -> Result<Vec<SearchResult>, Box<dyn std::error::Error>> {
        // Text-to-image search using CLIP
        let text_embedding = clip_embed_text(text_query);
        
        let coll = self.db.get_collection("images")?;
        
        let mut search = Search::new(&text_embedding).top_k(20);
        
        if let Some(tag) = tag_filter {
            search = search.filter(Filter::in_("tags", vec![tag]));
        }
        
        let results = coll.search(&search)?;
        
        Ok(results.into_iter().map(|m| SearchResult {
            image_path: m.id,
            similarity: m.score,
        }).collect())
    }
}
```

### Use Case 3: Recommendation System

```rust
pub struct RecommendationEngine {
    db: Database,
}

impl RecommendationEngine {
    pub fn new(data_dir: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let db = Database::open(data_dir)?;
        
        // User and item embeddings
        if !db.has_collection("items")? {
            db.create_collection("items", CollectionConfig::new(128))?;
        }
        
        Ok(Self { db })
    }
    
    pub fn add_item(
        &self,
        item_id: &str,
        embedding: Vec<f32>,
        category: &str,
        price_range: &str,
    ) -> Result<(), ndb::Error> {
        let coll = self.db.get_collection("items")?;
        coll.insert(Document {
            id: item_id.to_string(),
            vector: embedding,
            payload: Some(serde_json::json!({
                "category": category,
                "price_range": price_range,
            })),
        })?;
        Ok(())
    }
    
    pub fn recommend(
        &self,
        user_embedding: &[f32],
        category: Option<&str>,
        max_price: Option<f64>,
        n: usize,
    ) -> Result<Vec<Recommendation>, ndb::Error> {
        let coll = self.db.get_collection("items")?;
        
        // Build filter
        let mut filters = vec![];
        
        if let Some(cat) = category {
            filters.push(Filter::eq("category", cat));
        }
        
        if let Some(price) = max_price {
            filters.push(Filter::lte("price", price));
        }
        
        let search = Search::new(user_embedding)
            .top_k(n * 2)  // Request more for post-filtering
            .filter(if filters.is_empty() {
                Filter::And(vec![])
            } else if filters.len() == 1 {
                filters.pop().unwrap()
            } else {
                Filter::and(filters)
            });
        
        let results = coll.search(&search)?;
        
        Ok(results.into_iter().take(n).map(|m| Recommendation {
            item_id: m.id,
            score: m.score,
        }).collect())
    }
}
```

---

## Deployment Strategies

### Single-Node Deployment

```yaml
# docker-compose.yml
version: '3.8'
services:
  app:
    build: .
    volumes:
      - ndb-data:/data
    environment:
      - NDB_DATA_DIR=/data
      - NDB_DURABILITY=sync
    deploy:
      resources:
        limits:
          memory: 8G

volumes:
  ndb-data:
```

### Kubernetes StatefulSet

```yaml
# k8s-deployment.yaml
apiVersion: apps/v1
kind: StatefulSet
metadata:
  name: vector-service
spec:
  serviceName: vector-service
  replicas: 1  # nDB is single-writer
  selector:
    matchLabels:
      app: vector-service
  template:
    metadata:
      labels:
        app: vector-service
    spec:
      containers:
      - name: service
        image: myapp:latest
        env:
        - name: NDB_DATA_DIR
          value: "/data"
        volumeMounts:
        - name: data
          mountPath: /data
        resources:
          requests:
            memory: "4Gi"
            cpu: "2"
          limits:
            memory: "16Gi"
            cpu: "4"
  volumeClaimTemplates:
  - metadata:
      name: data
    spec:
      accessModes: ["ReadWriteOnce"]
      resources:
        requests:
          storage: 100Gi
```

### Read Replica Pattern

```yaml
# Primary (writer) deployment
apiVersion: apps/v1
kind: Deployment
metadata:
  name: vector-primary
spec:
  replicas: 1
  template:
    spec:
      containers:
      - name: primary
        image: myapp:writer
        volumeMounts:
        - name: shared-data
          mountPath: /data
      volumes:
      - name: shared-data
        persistentVolumeClaim:
          claimName: shared-pvc
---
# Reader deployment
apiVersion: apps/v1
kind: Deployment
metadata:
  name: vector-reader
spec:
  replicas: 3  # Scale readers horizontally
  template:
    spec:
      containers:
      - name: reader
        image: myapp:reader
        volumeMounts:
        - name: shared-data
          mountPath: /data
          readOnly: true  # Readers don't need write
      volumes:
      - name: shared-data
        persistentVolumeClaim:
          claimName: shared-pvc
          readOnly: true
```

---

## Performance Optimization

### Pre-allocate Collections

```rust
// Create collections upfront to avoid runtime creation overhead
let collections = vec![
    ("embeddings_768", 768),
    ("embeddings_1536", 1536),
    ("images", 512),
];

for (name, dim) in collections {
    if !db.has_collection(name)? {
        db.create_collection(name, CollectionConfig::new(dim))?;
    }
}
```

### Batch Processing

```rust
// Process in batches for optimal throughput
const BATCH_SIZE: usize = 1000;

for chunk in documents.chunks(BATCH_SIZE) {
    coll.insert_batch(chunk.to_vec())?;
}

// Flush after bulk load
coll.flush()?;

// Build index after all data loaded
coll.rebuild_index()?;
```

### Memory Mapping Tuning

```rust
// Advise kernel about access patterns
use memmap2::MmapOptions;

// For read-heavy workloads, prefetch can help
// (nDB does this automatically for segments)
```

### Connection Pooling

```rust
use deadpool::managed::{Manager, Pool};

struct NDBManager {
    db_path: String,
}

#[async_trait::async_trait]
impl Manager for NDBManager {
    type Type = Collection;
    type Error = ndb::Error;
    
    async fn create(&self) -> Result<Collection, ndb::Error> {
        let db = Database::open(&self.db_path)?;
        db.get_collection("default")
    }
    
    async fn recycle(&self, _: &mut Collection) -> deadpool::managed::RecycleResult<ndb::Error> {
        Ok(())
    }
}

// Usage
let pool = Pool::builder(NDBManager { db_path: "./data".to_string() })
    .max_size(16)
    .build()?;

let coll = pool.get().await?;
let results = coll.search(&search)?;
```

---

## Monitoring & Observability

### Metrics Collection

```rust
use prometheus::{Counter, Histogram, Registry};

pub struct NDBMetrics {
    search_duration: Histogram,
    insert_count: Counter,
    error_count: Counter,
}

impl NDBMetrics {
    pub fn new(registry: &Registry) -> Self {
        Self {
            search_duration: Histogram::with_opts(
                opts!("ndb_search_duration_seconds", "Search latency")
            ).unwrap(),
            insert_count: Counter::new(
                "ndb_inserts_total", "Total inserts"
            ).unwrap(),
            error_count: Counter::new(
                "ndb_errors_total", "Total errors"
            ).unwrap(),
        }
    }
    
    pub fn record_search<F, T>(&self, f: F) -> T
    where
        F: FnOnce() -> T,
    {
        let timer = self.search_duration.start_timer();
        let result = f();
        timer.observe_duration();
        result
    }
}
```

### Health Checks

```rust
pub async fn health_check(State(db): State<Arc<Database>>) -> impl IntoResponse {
    // Check if database is accessible
    match db.get_collection("health") {
        Ok(_) => (StatusCode::OK, "healthy"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "unhealthy"),
    }
}
```

### Tracing Integration

```rust
use tracing::{info, span, Level};

#[tracing::instrument(skip(coll))]
pub fn search_with_tracing(
    coll: &Collection,
    query: &[f32],
) -> Result<Vec<Match>, ndb::Error> {
    let span = span!(Level::INFO, "search", query_len = query.len());
    let _enter = span.enter();
    
    info!("Starting search");
    let start = Instant::now();
    
    let results = coll.search(&Search::new(query).top_k(10))?;
    
    info!(duration_ms = start.elapsed().as_millis(), "Search completed");
    
    Ok(results)
}
```

---

## Troubleshooting

### Common Issues

#### Issue: Slow Search Performance

**Symptoms:** Search takes >100ms on small datasets

**Diagnosis:**
```rust
// Check if HNSW index exists
if !coll.has_index() {
    println!("No HNSW index - building...");
    coll.rebuild_index()?;
}

// Verify segment count (too many = slow)
let manifest = coll.manifest()?;
println!("Active segments: {}", manifest.segments.len());
```

**Solutions:**
1. Build HNSW index: `coll.rebuild_index()`
2. Compact if many segments: `coll.compact()`
3. Increase `ef` parameter for approximate search
4. Check if dimension mismatch is causing fallback to exact search

#### Issue: CollectionLocked Error

**Symptoms:**
```
Error: CollectionLocked { name: "embeddings" }
```

**Causes:**
- Another process has the collection open
- Previous process crashed without releasing lock

**Solutions:**
```bash
# Check for orphaned lock files
ls -la data/embeddings/

# Remove LOCK file if no process is using it
rm data/embeddings/LOCK
```

#### Issue: High Memory Usage

**Diagnosis:**
```rust
// Check HNSW index size
if let Some(index) = coll.index_stats()? {
    println!("Index size: {} bytes", index.size_bytes);
}

// Check segment count
let stats = coll.stats()?;
println!("Segments: {}, Docs: {}", stats.segment_count, stats.doc_count);
```

**Solutions:**
1. Reduce HNSW `M` parameter
2. Compact collection: `coll.compact()`
3. Delete unnecessary index: `coll.delete_index()`
4. Reduce payload sizes

#### Issue: Corruption After Crash

**Symptoms:** Error opening database after power loss

**Solutions:**
1. nDB automatically truncates corrupt WAL records
2. Check filesystem integrity: `fsck` (Linux) or `chkdsk` (Windows)
3. Restore from backup if segments are corrupted

### Debug Logging

```rust
use tracing_subscriber;

fn main() {
    // Enable debug logging
    tracing_subscriber::fmt()
        .with_max_level(tracing::Level::DEBUG)
        .init();
    
    let db = Database::open("./data")?;
    // ... operations will be logged
}
```

---

## Migration Guide

### From Other Vector Databases

#### From FAISS

```rust
// Export from FAISS (Python)
import faiss
import numpy as np

index = faiss.read_index("index.faiss")
vectors = index.reconstruct_n(0, index.ntotal)
np.save("vectors.npy", vectors)

// Import to nDB
let vectors: Vec<Vec<f32>> = read_npy("vectors.npy");
for (i, vec) in vectors.iter().enumerate() {
    coll.insert(Document {
        id: format!("doc_{}", i),
        vector: vec.clone(),
        payload: None,
    })?;
}
```

#### From ChromaDB

```rust
// Export from ChromaDB
let chroma_docs = chroma_collection.get();

// Import to nDB
for doc in chroma_docs {
    coll.insert(Document {
        id: doc.id,
        vector: doc.embedding,
        payload: Some(doc.metadata),
    })?;
}
```

---

## Appendix: API Quick Reference

| Operation | Method | Notes |
|-----------|--------|-------|
| Open DB | `Database::open(path)` | Creates if not exists |
| Create Collection | `db.create_collection(name, config)` | Fails if exists |
| Get Collection | `db.get_collection(name)` | Returns existing |
| Insert | `coll.insert(doc)` | Replaces if ID exists |
| Batch Insert | `coll.insert_batch(docs)` | Atomic batch |
| Get by ID | `coll.get(id)` | Returns Option |
| Delete | `coll.delete(id)` | Soft delete |
| Search | `coll.search(&search)` | Exact or approximate |
| Flush | `coll.flush()` | Memtable → Segment |
| Compact | `coll.compact()` | Remove deleted, rebuild index |
| Build Index | `coll.rebuild_index()` | Creates HNSW |
| Delete Index | `coll.delete_index()` | Removes HNSW |

---

*For additional support, see the [GitHub repository](https://github.com/ndb/ndb) or file an issue.*
