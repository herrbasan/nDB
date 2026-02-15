# nDB N-API Integration (Node.js Native)

> **Full native performance for Node.js applications**  
> **Version:** 0.1.0  
> **Last Updated:** 2026-02-15

---

## Overview

The N-API integration provides **native-speed** access to nDB from Node.js. This is the most efficient approach when you need maximum performance and can use native modules.

### When to Use N-API

| Scenario | N-API is ideal |
|----------|----------------|
| **API servers** | Express, Fastify, NestJS backends |
| **High throughput** | >50K inserts/second, <1ms search latency |
| **File persistence** | Memory-mapped I/O, instant recovery |
| **SIMD acceleration** | AVX2/AVX-512 for distance computation |

### Key Features

- **Native performance**: Zero overhead over Rust implementation
- **Memory-mapped files**: Instant recovery regardless of dataset size
- **Full SIMD support**: AVX2, AVX-512, NEON on all platforms
- **Single/multi-threaded**: Works with Node.js event loop
- **Cross-platform**: Prebuilt binaries for Windows, macOS, Linux (x64, ARM64)

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                    Node.js Process                          │
│  ┌─────────────────┐      ┌──────────────────────────────┐ │
│  │   Your JS Code  │◄────►│   ndb.node (N-API addon)     │ │
│  │                 │      │   ┌────────────────────────┐ │ │
│  │  const db =     │      │   │ napi-rs bindings       │ │ │
│  │    new NDB()    │      │   │ • Type conversions     │ │ │
│  │                 │      │   │ • Error mapping        │ │ │
│  │  db.search(...) │      │   │ • Buffer handling      │ │ │
│  │                 │      │   └────────────────────────┘ │ │
│  └─────────────────┘      │              │               │ │
│                           │   ┌──────────┴──────────┐    │ │
│                           │   ▼                     ▼    │ │
│                           │ ┌────────────┐    ┌─────────┐│ │
│                           │ │ ndb crate  │    │  std    ││ │
│                           │ │ • Database │    │  libs   ││ │
│                           │ │ • Search   │    │         ││ │
│                           │ │ • HNSW     │    │         ││ │
│                           │ └────────────┘    └─────────┘│ │
│                           └──────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│                 Operating System                            │
│        ┌──────────────────────────────────────┐             │
│        │    Memory-Mapped Files (mmap)        │             │
│        │    • Zero-copy reads                 │             │
│        │    • Instant recovery                │             │
│        └──────────────────────────────────────┘             │
└─────────────────────────────────────────────────────────────┘
```

---

## Installation

```bash
npm install ndb-node
```

Prebuilt binaries are automatically downloaded for your platform. If a prebuilt binary isn't available, it will compile from source (requires Rust toolchain).

### Platform Support

| Platform | Architecture | Status |
|----------|--------------|--------|
| Linux | x64, ARM64 | ✅ Prebuilt |
| macOS | x64, ARM64 (Apple Silicon) | ✅ Prebuilt |
| Windows | x64 | ✅ Prebuilt |
| FreeBSD | x64 | ⚠️ Compile from source |

---

## Quick Start

```javascript
const { Database, FilterBuilder } = require('ndb-node');

// Open or create database
const db = new Database('./data');

// Create collection for 1536-dim embeddings (OpenAI, etc.)
const collection = db.createCollection('documents', 1536, {
  durability: 'sync'  // 'sync' or 'buffered'
});

// Insert documents
const doc = {
  id: 'doc1',
  vector: new Array(1536).fill(0.1),  // Your embedding
  payload: { title: 'Hello World', category: 'intro' }
};

collection.insert(doc.id, doc.vector, JSON.stringify(doc.payload));

// Build HNSW index for fast approximate search
collection.rebuildIndex();

// Search
const results = collection.search({
  vector: new Array(1536).fill(0.15),
  topK: 10,
  approximate: true,
  ef: 64
});

console.log(results);
// [{ id: 'doc1', score: 0.95, payload: '{"title":"Hello World"...}' }]
```

---

## API Reference

### Database

```javascript
const { Database } = require('ndb-node');

// Open or create database at path
const db = new Database('./data');

// Create a collection
const collection = db.createCollection(
  'my-collection',    // name
  768,                // vector dimension
  { durability: 'sync' }  // options (optional)
);

// Get existing collection
const existing = db.getCollection('my-collection');

// List all collections
const names = db.listCollections();  // ['my-collection', ...]
```

**Options:**
- `durability`: `'buffered'` (fast, 5-30s window) or `'sync'` (slower, guaranteed)

### Collection

```javascript
// Insert single document
collection.insert(
  'doc-id',                              // id
  [0.1, 0.2, 0.3, /* ... */],           // vector (array of floats)
  JSON.stringify({ title: 'My Doc' })    // payload (optional JSON string)
);

// Batch insert (much faster)
collection.insertBatch([
  { id: 'doc1', vector: vec1, payload: JSON.stringify(payload1) },
  { id: 'doc2', vector: vec2, payload: JSON.stringify(payload2) },
  // ... hundreds more
]);

// Retrieve by ID
const doc = collection.get('doc-id');
// { id: 'doc-id', vector: [...], payload: '{"title":"My Doc"}' }

// Delete
collection.delete('doc-id');

// Search
const results = collection.search({
  vector: queryVector,      // Array of floats
  topK: 10,                 // Number of results (default: 10)
  distance: 'cosine',       // 'cosine', 'dot', or 'euclidean'
  approximate: true,        // Use HNSW (requires index)
  ef: 64,                   // HNSW quality parameter
  filter: JSON.stringify(FilterBuilder.eq('category', 'tech'))
});

// Results: [{ id: string, score: number, payload?: string }]
```

### Maintenance Operations

```javascript
// Flush memtable to disk (creates new segment)
collection.flush();

// Compact segments and rebuild index
collection.compact();

// Build HNSW index for approximate search
collection.rebuildIndex();

// Delete HNSW index
collection.deleteIndex();

// Check if index exists
const hasIndex = collection.hasIndex();

// Force WAL sync to disk
collection.sync();

// Get collection info
console.log(collection.name);      // 'my-collection'
console.log(collection.config);    // { dim: 768, durability: 'sync' }
```

### FilterBuilder

```javascript
const { FilterBuilder } = require('ndb-node');

// Equality
FilterBuilder.eq('status', 'active')
// { status: { $eq: 'active' } }

// Comparisons
FilterBuilder.gt('score', 0.5)     // greater than
FilterBuilder.gte('score', 0.5)    // greater than or equal
FilterBuilder.lt('age', 100)       // less than
FilterBuilder.lte('age', 100)      // less than or equal
FilterBuilder.ne('status', 'deleted')  // not equal

// Array contains
FilterBuilder.in('tags', ['important', 'featured'])
// { tags: { $in: ['important', 'featured'] } }

// Combine with AND/OR
FilterBuilder.and(
  FilterBuilder.eq('status', 'active'),
  FilterBuilder.gt('score', 0.5)
)
// { $and: [{ status: { $eq: 'active' } }, { score: { $gt: 0.5 } }] }

FilterBuilder.or(
  FilterBuilder.eq('category', 'A'),
  FilterBuilder.eq('category', 'B')
)
```

---

## Complete Examples

### RAG (Retrieval-Augmented Generation)

```javascript
const { Database, FilterBuilder } = require('ndb-node');

class VectorStore {
  constructor(dataDir = './data') {
    this.db = new Database(dataDir);
  }

  initCollection(name, dimension) {
    try {
      return this.db.createCollection(name, dimension, { durability: 'sync' });
    } catch (e) {
      return this.db.getCollection(name);
    }
  }

  addDocument(collectionName, id, embedding, metadata) {
    const coll = this.db.getCollection(collectionName);
    coll.insert(id, embedding, JSON.stringify(metadata));
  }

  searchSimilar(collectionName, queryEmbedding, options = {}) {
    const coll = this.db.getCollection(collectionName);
    
    const filter = options.filter 
      ? JSON.stringify(options.filter) 
      : undefined;

    return coll.search({
      vector: queryEmbedding,
      topK: options.topK || 5,
      approximate: true,
      ef: options.ef || 64,
      filter
    });
  }
}

// Usage
const store = new VectorStore('./rag-data');
const docs = store.initCollection('documents', 1536);

// After ingesting documents...
docs.rebuildIndex();

// Search with filter
const results = store.searchSimilar('documents', queryVector, {
  topK: 5,
  filter: FilterBuilder.eq('source', 'handbook')
});
```

### Express.js API

```javascript
const express = require('express');
const { Database } = require('ndb-node');

const app = express();
app.use(express.json());

// Initialize once, reuse across requests
const db = new Database('./data');
const collection = db.getCollection('embeddings');

// Search endpoint
app.post('/search', (req, res) => {
  try {
    const { vector, topK = 10 } = req.body;
    
    const results = collection.search({
      vector,
      topK,
      approximate: true
    });

    res.json({ results });
  } catch (error) {
    res.status(500).json({ error: error.message });
  }
});

// Insert endpoint
app.post('/insert', (req, res) => {
  try {
    const { id, vector, payload } = req.body;
    
    collection.insert(id, vector, JSON.stringify(payload));
    
    res.json({ success: true });
  } catch (error) {
    res.status(500).json({ error: error.message });
  }
});

app.listen(3000, () => {
  console.log('API server on port 3000');
});
```

### Bulk Import Script

```javascript
const { Database } = require('ndb-node');
const fs = require('fs');
const readline = require('readline');

async function bulkImport(dataFile, collectionName, dimension) {
  const db = new Database('./import-data');
  
  // Create collection
  const coll = db.createCollection(collectionName, dimension, {
    durability: 'buffered'  // Fast for bulk import
  });

  const fileStream = fs.createReadStream(dataFile);
  const rl = readline.createInterface({
    input: fileStream,
    crlfDelay: Infinity
  });

  let batch = [];
  const BATCH_SIZE = 1000;
  let imported = 0;

  for await (const line of rl) {
    const record = JSON.parse(line);
    
    batch.push({
      id: record.id,
      vector: record.embedding,
      payload: JSON.stringify(record.metadata)
    });

    if (batch.length >= BATCH_SIZE) {
      coll.insertBatch(batch);
      imported += batch.length;
      console.log(`Imported ${imported}...`);
      batch = [];
    }
  }

  // Final batch
  if (batch.length > 0) {
    coll.insertBatch(batch);
    imported += batch.length;
  }

  // Flush and index
  coll.flush();
  console.log('Building HNSW index...');
  coll.rebuildIndex();

  console.log(`Done! Imported ${imported} documents.`);
}

// Usage: node import.js data.jsonl documents 768
bulkImport(process.argv[2], process.argv[3], parseInt(process.argv[4]));
```

---

## Performance Tuning

### 1. Batch Operations

```javascript
// Bad: Individual inserts
for (const doc of documents) {
  collection.insert(doc.id, doc.vector, doc.payload);
}

// Good: Batch insert
collection.insertBatch(documents);
collection.flush();
```

### 2. Index Strategy

```javascript
// For read-heavy workloads: build index once after bulk load
collection.insertBatch(documents);
collection.flush();
collection.rebuildIndex();

// For write-heavy workloads: periodic compaction
setInterval(() => {
  collection.compact();
}, 24 * 60 * 60 * 1000);  // Daily
```

### 3. Durability Selection

```javascript
// Bulk import: use buffered (faster)
const importColl = db.createCollection('import', 768, {
  durability: 'buffered'
});

// Production serving: use sync (safer)
const prodColl = db.createCollection('production', 768, {
  durability: 'sync'
});
```

### 4. Search Parameters

```javascript
// Speed vs Recall trade-off
const fastResults = collection.search({
  vector: query,
  topK: 10,
  approximate: true,
  ef: 32   // Lower = faster, lower recall
});

const accurateResults = collection.search({
  vector: query,
  topK: 10,
  approximate: true,
  ef: 200  // Higher = slower, higher recall
});
```

---

## Troubleshooting

### "Cannot find module './ndb.node'"

```bash
# Binary not available for your platform - rebuild
npm rebuild ndb-node

# Or force compile
npm install ndb-node --build-from-source
```

### "CollectionLocked" Error

```javascript
// Another process has the collection open
// Check for orphaned processes

// If you're sure no process is using it, check lock file:
const fs = require('fs');
const lockPath = './data/my-collection/LOCK';

if (fs.existsSync(lockPath)) {
  console.warn('Lock file exists. If no process is running, delete it manually.');
  // fs.unlinkSync(lockPath);  // Only if you're certain!
}
```

### Slow Search Performance

```javascript
// Check if HNSW index exists
if (!collection.hasIndex()) {
  console.log('Building index...');
  collection.rebuildIndex();
}

// Use approximate search
collection.search({
  vector: query,
  topK: 10,
  approximate: true  // Not exact!
});

// Check segment count (too many = slow)
const stats = collection.stats;
console.log('Segments:', stats.segmentCount);
// If > 10, compact:
collection.compact();
```

### High Memory Usage

```javascript
// Delete HNSW index if not needed
collection.deleteIndex();

// Compact to remove deleted documents
const result = collection.compact();
console.log(`Reclaimed space: ${result.docsBefore - result.docsAfter} docs`);
```

---

## Building from Source

If prebuilt binaries aren't available for your platform:

```bash
# Install Rust
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Install node-gyp dependencies
# macOS:
brew install python3 make

# Ubuntu/Debian:
sudo apt-get install build-essential python3

# Windows:
npm install --global windows-build-tools

# Build from source
npm install ndb-node --build-from-source
```

---

## License

MIT OR Apache-2.0 (same as nDB)
