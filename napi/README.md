# nDB Node.js Native Bindings

Native Node.js bindings for nDB - a high-performance embedded vector database.

## Installation via Git Submodule

```bash
# Add nDB as a submodule in your project
git submodule add https://github.com/ndb/ndb.git ndb
git submodule update --init --recursive

# Build the native module (REQUIRED - binary is not committed)
cd ndb/napi
node setup.js

# For Windows: copy/symlink the DLL to the expected name
copy target\release\ndb_node.dll napi\ndb-node.win32-x64-msvc.node
# Or on Unix:
# ln -s target/release/libndb_node.so napi/ndb-node.linux-x64-gnu.node
```

## Usage

```javascript
const { Database, FilterBuilder } = require('./ndb/napi');

// Open or create database
const db = new Database('./data');

// Create collection for 1536-dim embeddings (OpenAI, etc.)
const collection = db.createCollection('documents', 1536, {
  durability: 'sync'  // 'sync' or 'buffered'
});

// Insert documents
collection.insert('doc1', new Array(1536).fill(0.1), JSON.stringify({ title: 'Hello' }));

// Build HNSW index for fast approximate search
collection.rebuildIndex();

// Search
const results = collection.search({
  vector: new Array(1536).fill(0.15),
  topK: 10,
  distance: 'cosine',
  approximate: true,
  ef: 64
});

console.log(results);
// [{ id: 'doc1', score: 0.95, payload: '{"title":"Hello"}' }]
```

## Build Configuration

### Environment Variable

You can specify the native binary path directly:

```bash
# Windows
set NODE_NDB_NATIVE_PATH=D:\path\to\ndb_node.dll
node your-app.js

# Unix
export NODE_NDB_NATIVE_PATH=/path/to/libndb_node.so
node your-app.js
```

### Platform Binary Names

The loader looks for files matching these patterns:

| Platform | Architecture | Binary Name |
|----------|-------------|-------------|
| Windows | x64 | `ndb-node.win32-x64-msvc.node` |
| Windows | arm64 | `ndb-node.win32-arm64-msvc.node` |
| macOS | x64 | `ndb-node.darwin-x64.node` |
| macOS | arm64 | `ndb-node.darwin-arm64.node` |
| Linux | x64 | `ndb-node.linux-x64-gnu.node` |
| Linux | arm64 | `ndb-node.linux-arm64-gnu.node` |

If the file doesn't exist with the expected name, the loader also checks:
- `target/release/ndb_node.dll` (Windows)
- `target/release/libndb_node.so` (Linux)
- `target/release/libndb_node.dylib` (macOS)

## API Reference

### Database

```javascript
const db = new Database('./data');  // Open or create

const coll = db.createCollection(name, dimension, options);
const existing = db.getCollection(name);
const names = db.listCollections();
```

### Collection

```javascript
// Insert
collection.insert(id, vector, payload);
collection.insertBatch([{ id, vector, payload }, ...]);

// Retrieve/Delete
const doc = collection.get(id);
const deleted = collection.delete(id);

// Search
const results = collection.search({
  vector: queryVector,
  topK: 10,
  distance: 'cosine',  // 'cosine', 'dot', 'euclidean'
  approximate: true,   // Use HNSW (requires index)
  ef: 64,              // HNSW quality parameter
  filter: FilterBuilder.eq('category', 'books')
});

// Maintenance
collection.flush();        // Flush memtable to disk
collection.sync();         // Force WAL sync
collection.compact();      // Merge segments, reclaim space
collection.rebuildIndex(); // Build HNSW index
collection.deleteIndex();  // Remove HNSW index
collection.hasIndex();     // Check if index exists

// Properties
collection.name;   // Collection name
collection.config; // { dim, durability }
collection.stats;  // { memtableDocs, segmentCount, totalSegmentDocs }
```

### FilterBuilder

```javascript
const { FilterBuilder } = require('./ndb/napi');

// Comparison filters
FilterBuilder.eq('field', value);      // field == value
FilterBuilder.gt('field', value);      // field > value
FilterBuilder.gte('field', value);     // field >= value
FilterBuilder.lt('field', value);      // field < value
FilterBuilder.lte('field', value);     // field <= value
FilterBuilder.ne('field', value);      // field != value
FilterBuilder.in_('field', [values]);  // field IN values

// Logical operators
FilterBuilder.and([filter1, filter2]);
FilterBuilder.or([filter1, filter2]);
```

## Development

```bash
# Build debug version
cargo build -p ndb-node

# Build release version (optimized)
cargo build --release -p ndb-node

# Run example
cd napi
node examples/basic.js
```

## Project Structure (when used as submodule)

```
your-project/
├── ndb/                    # git submodule
│   ├── napi/              # this directory
│   │   ├── index.js       # loader
│   │   ├── index.d.ts     # TypeScript types
│   │   └── examples/      # examples
│   ├── src/               # Rust source
│   └── target/
│       └── release/
│           └── ndb_node.dll   # native binary
├── your-app.js
└── .gitmodules
```

## Requirements

- Node.js >= 16
- Rust >= 1.75

> **Note:** The native binary (`*.node`) is not committed to git. You must run `node setup.js` after cloning or pulling updates.
- For Windows: Visual Studio Build Tools or MSVC
- For Linux: GCC/Clang
- For macOS: Xcode Command Line Tools

## License

MIT OR Apache-2.0 (same as nDB)
