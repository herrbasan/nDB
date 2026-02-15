# nDB WebAssembly Integration

> **Browser and Edge-compatible vector search**  
> **Version:** 0.1.0  
> **Last Updated:** 2026-02-15

---

## ⚠️ Future / Optional Deployment

**Status:** Planned / Not yet implemented  
**Priority:** N-API and gRPC are the primary deployment targets

This integration is designed for environments where native modules cannot be used (browsers, certain edge platforms). It provides ~80-90% of native performance but with significant limitations:

- ❌ No file system access (in-memory only)
- ❌ Limited SIMD support (128-bit vs 256/512-bit)
- ❌ Single-threaded

**Use N-API or gRPC unless you specifically need browser/edge compatibility.**

---

## Overview

The WebAssembly (WASM) integration lets you run nDB in browsers, edge functions, and environments where native modules aren't available. It provides **~80-90% of native performance** with the convenience of universal deployment.

### When to Use WASM

| Scenario | WASM is ideal |
|----------|---------------|
| **Browser apps** | Client-side vector search, RAG UIs |
| **Edge functions** | Vercel Edge, Cloudflare Workers, Deno Deploy |
| **Restricted environments** | Where native modules are blocked |
| **Simple deployment** | Single `.wasm` file, no platform-specific binaries |

### Key Features

- **Universal runtime**: Works in any WASM-compatible environment
- **No native dependencies**: Single `.js` + `.wasm` bundle
- **Sandboxed**: Safe for untrusted environments
- **Small footprint**: ~2MB gzipped (vs ~5MB for native)

### Limitations

| Feature | Native (N-API) | WASM |
|---------|---------------|------|
| File persistence | ✅ mmap | ❌ In-memory only |
| SIMD performance | ✅ AVX2/AVX-512 | ⚠️ SIMD128 (128-bit) |
| Threading | ✅ Multi-thread | ❌ Single-threaded |
| Startup time | Fast | Medium (compilation) |

---

## Architecture

```
┌─────────────────────────────────────────────────────────────┐
│                  Browser / Edge Runtime                     │
│                                                             │
│  ┌─────────────────┐      ┌──────────────────────────────┐ │
│  │   Your JS Code  │◄────►│   ndb_wasm.js (glue)         │ │
│  │                 │      │   ┌────────────────────────┐ │ │
│  │  const db =     │      │   │ wasm-bindgen bindings  │ │ │
│  │    new WasmDB() │      │   │ • JS/WASM bridge       │ │ │
│  │                 │      │   │ • Memory management    │ │ │
│  │  db.search(...) │      │   │ • API wrappers         │ │ │
│  │                 │      │   └────────────────────────┘ │ │
│  └─────────────────┘      │              │               │ │
│                           │   ┌──────────┴──────────┐    │ │
│                           │   ▼                     ▼    │ │
│                           │ ┌────────────┐    ┌─────────┐│ │
│                           │ │ ndb_wasm   │    │  std   ││ │
│                           │ │ .wasm      │    │  wasm  ││ │
│                           │ │ • Database │    │        ││ │
│                           │ │ • Search   │    │        ││ │
│                           │ │ • HNSW     │    │        ││ │
│                           │ └────────────┘    └─────────┘│ │
│                           └──────────────────────────────┘ │
└─────────────────────────────────────────────────────────────┘
                              │
                              ▼
┌─────────────────────────────────────────────────────────────┐
│              No File System Access (Sandboxed)              │
│                                                             │
│  • Data stored in WASM linear memory                        │
│  • Export to save, import to restore                        │
│  • LocalStorage/IndexedDB for persistence (browser)         │
└─────────────────────────────────────────────────────────────┘
```

---

## Installation

### npm

```bash
npm install ndb-wasm
```

### CDN (Browser)

```html
<script type="module">
  import init, { WasmDB } from 'https://cdn.jsdelivr.net/npm/ndb-wasm@0.1.0/pkg/ndb_wasm.js';
  
  await init();
  const db = new WasmDB();
</script>
```

### ES Modules (Deno, etc.)

```javascript
import init, { WasmDB } from 'npm:ndb-wasm';

await init();
const db = new WasmDB();
```

---

## Quick Start

```javascript
import init, { WasmDB } from 'ndb-wasm';

async function main() {
  // Initialize WASM module
  await init();
  
  // Create in-memory database
  const db = new WasmDB();
  
  // Create collection
  const collection = db.createCollection('documents', 384);
  
  // Insert documents
  collection.insert('doc1', 
    new Float32Array(384).fill(0.1),  // Your embedding
    JSON.stringify({ title: 'Hello' })
  );
  
  // Search
  const results = collection.search(
    new Float32Array(384).fill(0.15),
    10  // topK
  );
  
  console.log(results);
  // [{ id: 'doc1', score: 0.95, payload: '{"title":"Hello"}' }]
  
  // Export data (for persistence)
  const data = db.export();
  localStorage.setItem('my-db', toBase64(data));
}

main();
```

---

## API Reference

### Initialization

```javascript
import init, { WasmDB } from 'ndb-wasm';

// Must call init() before using any WASM functions
await init();

// Create database
const db = new WasmDB();
```

**Note:** `init()` downloads and compiles the `.wasm` file. Cache it for subsequent loads.

### Database

```javascript
// Create a collection
const collection = db.createCollection(
  'my-collection',  // name
  768               // vector dimension
);

// List collections
const names = db.listCollections();

// Export database to Uint8Array
const data = db.export();
// Save to localStorage, IndexedDB, etc.

// Import from Uint8Array
const restored = WasmDB.import(data);
```

### Collection

```javascript
// Insert document
collection.insert(
  'doc-id',                              // id (string)
  new Float32Array([0.1, 0.2, 0.3, ...]), // vector (Float32Array)
  JSON.stringify({ title: 'My Doc' })    // payload (optional JSON string)
);

// Batch insert (recommended)
collection.insertBatch([
  ['doc1', new Float32Array([...]), JSON.stringify({...})],
  ['doc2', new Float32Array([...]), JSON.stringify({...})],
  // ...
]);

// Get by ID
const doc = collection.get('doc-id');
// { id: 'doc-id', vector: Float32Array, payload: string }

// Delete
collection.delete('doc-id');

// Search
const results = collection.search(
  queryVector,    // Float32Array
  topK,           // number
  options         // { distance?: 'cosine'|'dot'|'euclidean', ef?: number }
);

// Results: [{ id: string, score: number, payload?: string }]
```

### Search Options

```javascript
// Basic search
const results = collection.search(queryVector, 10);

// With options
const results = collection.search(queryVector, 10, {
  distance: 'cosine',    // 'cosine', 'dot', or 'euclidean'
  ef: 64                 // HNSW quality (if index exists)
});
```

### Index Management

```javascript
// Build HNSW index for approximate search
collection.buildIndex({
  m: 16,              // Connections per layer
  efConstruct: 32     // Construction quality
});

// Check if index exists
const hasIndex = collection.hasIndex();

// Delete index
collection.deleteIndex();

// Get stats
const stats = collection.stats();
// { docCount: number, dimension: number, hasIndex: boolean }
```

---

## Complete Examples

### Browser RAG Application

```html
<!DOCTYPE html>
<html>
<head>
  <title>Vector Search Demo</title>
</head>
<body>
  <input type="file" id="fileInput" accept=".json" />
  <input type="text" id="query" placeholder="Search..." />
  <button id="searchBtn">Search</button>
  <div id="results"></div>

  <script type="module">
    import init, { WasmDB } from 'https://cdn.jsdelivr.net/npm/ndb-wasm@0.1.0/pkg/ndb_wasm.js';

    let db;
    let collection;

    async function initDB() {
      await init();
      
      // Try to restore from localStorage
      const saved = localStorage.getItem('vector-db');
      if (saved) {
        const data = Uint8Array.from(atob(saved), c => c.charCodeAt(0));
        db = WasmDB.import(data);
        collection = db.getCollection('documents');
        console.log('Restored from localStorage');
      } else {
        db = new WasmDB();
        collection = db.createCollection('documents', 384);
      }
    }

    // Load documents from JSON
    document.getElementById('fileInput').addEventListener('change', async (e) => {
      const file = e.target.files[0];
      const text = await file.text();
      const documents = JSON.parse(text);

      // Batch insert
      const batch = documents.map(doc => [
        doc.id,
        new Float32Array(doc.embedding),
        JSON.stringify(doc.metadata)
      ]);
      
      collection.insertBatch(batch);
      collection.buildIndex();

      // Save to localStorage
      const data = db.export();
      localStorage.setItem('vector-db', btoa(String.fromCharCode(...data)));
      
      console.log(`Loaded ${documents.length} documents`);
    });

    // Search
    document.getElementById('searchBtn').addEventListener('click', () => {
      const queryText = document.getElementById('query').value;
      
      // In real app, you'd call an embedding API here
      const queryVector = generateEmbedding(queryText);
      
      const results = collection.search(queryVector, 5);
      
      const resultsDiv = document.getElementById('results');
      resultsDiv.innerHTML = results.map(r => {
        const meta = JSON.parse(r.payload);
        return `<div>${meta.title} (score: ${r.score.toFixed(3)})</div>`;
      }).join('');
    });

    // Initialize
    initDB();

    function generateEmbedding(text) {
      // Placeholder - call OpenAI, etc.
      return new Float32Array(384).fill(Math.random());
    }
  </script>
</body>
</html>
```

### Vercel Edge Function

```javascript
// api/search.js
import init, { WasmDB } from 'ndb-wasm';

let dbPromise;

async function getDB() {
  if (!dbPromise) {
    dbPromise = init().then(() => {
      // In edge functions, we typically load from KV or fetch
      return new WasmDB();
    });
  }
  return dbPromise;
}

export const config = {
  runtime: 'edge',
};

export default async function handler(request) {
  const { query, topK = 10 } = await request.json();
  
  const db = await getDB();
  
  // In production, load collection from KV store
  // const data = await fetch('https://.../embeddings.bin').then(r => r.arrayBuffer());
  // const collection = WasmDB.import(new Uint8Array(data)).getCollection('docs');
  
  // For demo, create empty
  const collection = db.createCollection('docs', 768);
  
  // Search
  const results = collection.search(new Float32Array(query), topK);
  
  return new Response(JSON.stringify({ results }), {
    headers: { 'Content-Type': 'application/json' }
  });
}
```

### Cloudflare Worker

```javascript
// worker.js
import init, { WasmDB } from 'ndb-wasm';

let wasmModule;

export default {
  async fetch(request, env, ctx) {
    // Initialize WASM on first request
    if (!wasmModule) {
      await init();
      wasmModule = true;
    }

    const url = new URL(request.url);
    
    if (url.pathname === '/search') {
      const { vector, topK } = await request.json();
      
      // Load from R2 or KV
      const data = await env.VECTOR_STORE.get('db', { type: 'arrayBuffer' });
      const db = data ? WasmDB.import(new Uint8Array(data)) : new WasmDB();
      
      const collection = db.getCollection('documents') || db.createCollection('documents', 768);
      
      const results = collection.search(new Float32Array(vector), topK);
      
      return new Response(JSON.stringify({ results }), {
        headers: { 'Content-Type': 'application/json' }
      });
    }

    return new Response('Not found', { status: 404 });
  }
};
```

### React Hook

```javascript
// hooks/useVectorDB.js
import { useEffect, useState, useCallback } from 'react';
import init, { WasmDB } from 'ndb-wasm';

export function useVectorDB(dimension = 768) {
  const [db, setDb] = useState(null);
  const [collection, setCollection] = useState(null);
  const [ready, setReady] = useState(false);

  useEffect(() => {
    let mounted = true;

    async function initDB() {
      await init();
      
      if (!mounted) return;
      
      const newDb = new WasmDB();
      const coll = newDb.createCollection('default', dimension);
      
      setDb(newDb);
      setCollection(coll);
      setReady(true);
    }

    initDB();

    return () => { mounted = false; };
  }, [dimension]);

  const insert = useCallback((id, vector, metadata) => {
    if (!collection) return;
    collection.insert(id, vector, JSON.stringify(metadata));
  }, [collection]);

  const search = useCallback((queryVector, topK = 10) => {
    if (!collection) return [];
    return collection.search(queryVector, topK);
  }, [collection]);

  const buildIndex = useCallback(() => {
    if (!collection) return;
    collection.buildIndex();
  }, [collection]);

  return { ready, insert, search, buildIndex, db };
}

// Usage in component
function SearchComponent() {
  const { ready, insert, search, buildIndex } = useVectorDB(384);
  const [results, setResults] = useState([]);

  const handleSearch = async (query) => {
    if (!ready) return;
    
    // Get embedding from API
    const embedding = await fetchEmbedding(query);
    
    const matches = search(embedding, 5);
    setResults(matches);
  };

  return (
    <div>
      {!ready && <p>Loading...</p>}
      <button onClick={buildIndex}>Build Index</button>
      {/* ... */}
    </div>
  );
}
```

---

## Persistence Patterns

Since WASM can't access the filesystem directly, use these patterns:

### Browser: localStorage/IndexedDB

```javascript
// Save
function saveDB(db) {
  const data = db.export();
  const base64 = btoa(String.fromCharCode(...data));
  localStorage.setItem('vector-db', base64);
}

// Load
function loadDB() {
  const saved = localStorage.getItem('vector-db');
  if (!saved) return new WasmDB();
  
  const data = Uint8Array.from(atob(saved), c => c.charCodeAt(0));
  return WasmDB.import(data);
}
```

### IndexedDB (for larger datasets)

```javascript
// IndexedDB can store much more than localStorage
async function saveToIndexedDB(db) {
  const data = db.export();
  
  const request = indexedDB.open('VectorDB', 1);
  request.onupgradeneeded = (e) => {
    e.target.result.createObjectStore('databases');
  };
  
  const db = await new Promise((resolve, reject) => {
    request.onsuccess = () => resolve(request.result);
    request.onerror = () => reject(request.error);
  });
  
  const tx = db.transaction('databases', 'readwrite');
  tx.objectStore('databases').put(data, 'main');
  
  return new Promise((resolve, reject) => {
    tx.oncomplete = resolve;
    tx.onerror = () => reject(tx.error);
  });
}
```

### Edge: KV Store

```javascript
// Cloudflare Workers KV
await env.VECTOR_STORE.put('db', db.export());

// Retrieve
const data = await env.VECTOR_STORE.get('db', { type: 'arrayBuffer' });
const db = WasmDB.import(new Uint8Array(data));
```

---

## Performance Considerations

### Startup Time

WASM needs to be downloaded and compiled on first load:

```javascript
// Preload for faster startup
<link rel="preload" href="/ndb_wasm.wasm" as="fetch" crossorigin>

// Or use a service worker to cache
```

### Memory Management

```javascript
// WASM has its own memory heap
// Large datasets may hit browser memory limits (~2-4GB)

// Monitor collection size
const stats = collection.stats();
if (stats.docCount > 100000) {
  console.warn('Approaching memory limit');
}
```

### SIMD Support

Check if SIMD is available for better performance:

```javascript
// Modern browsers support SIMD128
// Fallback to scalar implementation if not available

const hasSIMD = WebAssembly.validate(new Uint8Array([
  0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
  0x01, 0x05, 0x01, 0x60, 0x01, 0x7b, 0x01, 0x7b,
  0x03, 0x02, 0x01, 0x00, 0x0a, 0x0a, 0x01,
  0x08, 0x00, 0x20, 0x00, 0x20, 0x00, 0xfd, 0x0f,
  0x0b
]));

console.log('SIMD available:', hasSIMD);
```

---

## Building from Source

```bash
# Clone nDB
git clone https://github.com/ndb/ndb
cd ndb/wasm

# Install wasm-pack
curl https://rustwasm.github.io/wasm-pack/installer/init.sh -sSf | sh

# Build
wasm-pack build --target web --release

# Output in pkg/ directory
```

---

## Troubleshooting

### "WebAssembly module failed to initialize"

```javascript
// Ensure proper CORS headers on the .wasm file
// Server must serve with: Content-Type: application/wasm

// Or use bundled version
import init from 'ndb-wasm';  // Bundler handles the wasm file
```

### "Memory access out of bounds"

```javascript
// WASM memory is pre-allocated and can run out
// For large datasets, increase initial memory:

// In wasm-pack build, modify Cargo.toml:
// [package.metadata.wasm-pack.profile.release]
// wasm-opt = ["-O4", "--enable-mutable-globals"]
```

### Slow performance

```javascript
// Build HNSW index for large datasets
if (collection.stats().docCount > 1000) {
  collection.buildIndex();
}

// Use Float32Array (not regular arrays) for vectors
const vector = new Float32Array([0.1, 0.2, ...]);  // Fast
const slow = [0.1, 0.2, ...];  // Slow - needs conversion
```

---

## License

MIT OR Apache-2.0 (same as nDB)
