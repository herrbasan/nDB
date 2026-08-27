# nDB Node.js API Reference

> Complete API documentation for the `ndb` Node.js native addon.---

## Installation

```bash
npm install ndb
```

The package includes prebuilt native binaries for:
- Windows x64
- macOS x64 / arm64
- Linux x64

---

## Quick Start

```js
const { Database } = require('ndb');

// Open a database. nDB is database-as-a-folder: the path is the
// data.jsonl file *inside* the database folder.
const db = new Database('./mydata/data.jsonl');

// Insert
const id = db.insert({ name: 'Alice', age: 30, email: 'alice@example.com' });
console.log('Inserted:', id);

// Get by ID
const doc = db.get(id);
console.log('Document:', doc);

// Update
db.update(id, { name: 'Alice Smith', age: 31, email: 'alice@example.com' });

// Query
const active = await db.query({ status: { $eq: 'active' } });
console.log('Active users:', active.length);

// Delete
db.delete(id);
```

---

## `Database`

### Constructor

#### `new Database(path)`

Open or create a database. `path` is the `data.jsonl` file *inside* a database folder — the folder's `_files/` and `_trash/` are created implicitly.

```js
const db = new Database('./data/app/data.jsonl');
```

#### `Database.open(path, options?)`

Open with configuration options.

```js
const db = Database.open('./data/app/data.jsonl', {
    persistence: 'immediate'
});

// Scheduled flush every 60 seconds
const db2 = Database.open('./data/app/data.jsonl', {
    persistence: 'scheduled',
    interval: 60
});
```

**Options:**

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `persistence` | `string` | `'lazy'` | `'lazy'`, `'immediate'`, or `'scheduled'` |
| `interval` | `number` | `60` | Seconds between flushes (for `scheduled` mode) |
| `trash_ttl` | `number` | `undefined` | Auto-empty trash TTL in seconds (e.g., 86400 for 1 day) |
| `trash_purge_interval` | `number` | `3600` | Background loop interval in seconds (default 1 hour) |

#### `Database.openInMemory()`

Open a purely in-memory database (no file).

```js
const db = Database.openInMemory();
```

---

## Layer 1: Core Operations

### `insert(doc) → string`

Insert a document. Returns the generated `_id`.

```js
const id = db.insert({
    title: 'My Document',
    tags: ['important', 'review'],
    metadata: { created: Date.now() }
});
// id = "V1StGXR8Z5jdHi6B"
```

### `insertWithPrefix(prefix, doc) → string`

Insert with a prefixed ID.

```js
const id = db.insertWithPrefix('user', { name: 'Bob' });
// id = "user_k8Tm2pQw4xNvRj7L"
```

### `get(id) → object`

Get a document by ID. **Throws** if the ID does not exist or the document has been soft-deleted — it does **not** return `null`.

```js
const doc = db.get(id);
console.log(doc.name);
```

To read a key that may or may not exist, use `contains()` or catch:

```js
function readOptional(id) {
  try { return db.get(id); } catch { return null; }
}
```

See [Common Patterns](./common-patterns.md) for the singleton/config-record idiom that builds on this.

### `update(id, newDoc) → void`

Replace a document. The `_id` field is preserved.

```js
db.update(id, { name: 'Updated Name', age: 32 });
```
### `arrayPush(id, field, value) -> void`

Append a single element to a top-level array field. This creates a highly optimized delta write to the JSON Lines file rather than rewriting the entire document, which is critical for large documents like conversations.

```js
db.arrayPush(id, 'messages', { role: 'user', content: 'Hello' });
```

### `set(id, path, value) -> void`

Set a value at a dot-separated path within a document. Creates a tiny delta patch instead of rewriting the entire document.

- Path uses `.` to separate segments (e.g. `'messages.3.content'`)
- Numeric segments address array elements by index
- Creates new fields if the leaf key doesn't exist
- Unresolvable paths are silently skipped

```js
// Top-level field
db.set(id, 'title', 'New Title');

// Nested field
db.set(id, 'settings.theme', 'dark');

// Array element by index
db.set(id, 'messages.1.text', 'edited text');

// Any JSON value type
db.set(id, 'counter', 42);
db.set(id, 'active', true);
db.set(id, 'tags', ['a', 'b']);
db.set(id, 'metadata', null);
```

### `remove(id, path) -> void`

Remove a field or array element at a dot-separated path. Creates a tiny delta patch instead of rewriting the entire document.

- For object fields: the key is removed
- For array elements: the element is removed and remaining elements shift

```js
// Remove top-level field
db.remove(id, 'temporary_data');

// Remove nested field
db.remove(id, 'settings.volume');

// Remove array element (shifts remaining)
db.remove(id, 'messages.2');
```
### `delete(id) → void`

Soft delete a document (tombstone).

```js
db.delete(id);
```

### `contains(id) → boolean`

Check if a document exists.

```js
if (db.contains(id)) {
    console.log('Document exists');
}
```

### `len() → number`

Number of active documents.

```js
console.log('Total docs:', db.len());
```

### `isEmpty() → boolean`

Check if database is empty.

```js
if (db.isEmpty()) {
    console.log('No documents');
}
```

---

## Layer 2: Field Queries

### `find(field, value) → object[]`

Find all documents where `field` equals `value`. Uses index if available.

```js
const alices = db.find('name', 'Alice');
const activeUsers = db.find('status', 'active');
```

### `findRange(field, min, max) → object[]`

Find documents with field value in range [min, max]. Works best with a BTree index.

```js
const midRange = db.findRange('score', 50, 100);
```

### `iter() → object[]`

Get all active (non-deleted) documents.

```js
const allDocs = db.iter();
```

> ⚠️ **Size limit:** `iter()` marshals the entire database as one JSON string across the napi boundary. Very large databases can exceed napi's string size limit (~2 GB) and throw. For bulk reads on large corpora, page with `queryWith({ limit, offset })` instead (each page crosses the boundary separately).

---

## Layer 3: JSON AST Queries

### `query(ast) → object[]`

Execute a JSON AST query. See [Query Language Reference](./query-language.md) for full syntax.

```js
// Simple equality
await db.query({ status: 'active' });

// Comparison operators
await db.query({ age: { $gte: 21 } });

// Combined conditions (implicit AND)
await db.query({
    status: 'active',
    age: { $gte: 18 }
});

// Logical combinators
await db.query({
    $or: [
        { role: 'admin' },
        { role: 'moderator' }
    ]
});

// Nested combinators
await db.query({
    $and: [
        { $or: [{ status: 'active' }, { status: 'pending' }] },
        { age: { $gte: 18 } }
    ]
});
```

### `queryWith(ast, options) → object[]`

Query with limit, offset, and sort.

```js
const results = await db.queryWith(
    { status: 'active' },
    {
        sortBy: 'age',
        sortDir: 'desc',
        limit: 10,
        offset: 20
    }
);
```

**Options:**

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `sortBy` | `string` | — | Field name to sort by |
| `sortDir` | `string` | `'asc'` | `'asc'` or `'desc'` |
| `limit` | `number` | — | Maximum results to return |
| `offset` | `number` | `0` | Number of results to skip |

---

## Index Management

### `createIndex(field) → void`

Create a hash index on a field. Scans all existing documents.

```js
db.createIndex('email');
// Now db.find('email', 'alice@example.com') is O(1)
```

### `createBTreeIndex(field) → void`

Create a BTree index for range queries.

```js
db.createBTreeIndex('score');
// Now db.findRange('score', 50, 100) is optimized
```

### `dropIndex(field) → void`

Drop an index, freeing memory.

```js
db.dropIndex('email');
```

### `hasIndex(field) → boolean`

Check if an index exists for a field.

```js
if (!db.hasIndex('email')) {
    db.createIndex('email');
}
```

---

## Compaction & Trash

### `compact() → void`

Rewrite the database file, keeping only active documents. Archives deleted documents to trash.

```js
db.compact();
```

### `restore(id) → void`

Restore a soft-deleted document.

```js
db.delete(id);
// ... later ...
db.restore(id);
```

### `deletedIds() → string[]`

List all deleted document IDs.

```js
const deleted = db.deletedIds();
console.log('Deleted:', deleted);
```

---

## Persistence

### `flush() → void`

Explicitly flush pending writes to disk (fsync).

```js
db.flush();
```

---

## File Buckets

The Node.js N-API wrapper exposes flat file bucket methods directly on the `Database` instance (unlike the Rust API which uses `db.bucket(name)` bridging).

### `storeFile(bucket, name, data, mimeType) → FileMeta`

Store binary data in a bucket. Files are deduplicated via SHA-256.

```js
const fs = require('fs');
const imageData = fs.readFileSync('./photo.png');
// Parameters: bucketName, originalFileName, bufferData, mimeType
const meta = db.storeFile('avatars', 'photo.png', imageData, 'image/png');
console.log(meta);
// { _file: { bucket: 'avatars', id: 'a1b2c3d4', ext: 'png' },
//   name: 'photo.png', size: 12345, type: 'image/png', created: 1711553200 }
```

### `getFile(bucket, hash, ext) → Buffer`

Retrieve file content by hash and extension.

```js
const buffer = db.getFile('avatars', 'a1b2c3d4', 'png');
fs.writeFileSync('./retrieved.png', buffer);
```

### `restoreFile(bucket, hash, ext) → boolean`

Restore a file from bucket trash back to the active bucket. Returns `true` if the file was in trash and restored, `false` if it was not in trash (no-op).

```js
const restored = db.restoreFile('avatars', 'a1b2c3d4', 'png');
```

### `releaseFile(fileRefStr) → boolean`

Safely soft-delete a file based on its `nURI` string representation (e.g. `bucket:hash.ext`). 

Before deleting, `nDB` iteratively checks all active documents in memory. If no document references the file string in any value, the physical file is moved to the `_trash` folder. If *any* document still references it, the file is kept untouched. Returns `true` if the file was trashed, `false` otherwise.

```js
const wasTrashed = db.releaseFile('avatars:a1b2c3d4.png');
```

### `deleteFile(bucket, hash, ext) → void`

Immediate (hard) delete of a file directly from the bucket (moves to trash without checking references).

```js
db.deleteFile('avatars', 'a1b2c3d4', 'png');
```

### `listFiles(bucket) → string[]`

List all active files in a bucket.

```js
const files = db.listFiles('avatars');
files.forEach(f => console.log(f));
```

### `gcBuckets() → number`

Perform garabage collection on all file buckets. Iterates over all files stored in `_files/` and moves any unreferenced file into `_trash/files/`. Returns the number of files successfully trashed.

```js
const trashedCount = db.gcBuckets();
console.log(`Garbage collection trashed ${trashedCount} files.`);
```

---

## Error Handling

All methods throw on error. Use try/catch:

```js
try {
    const doc = db.get('nonexistent');
} catch (err) {
    console.error('Error:', err.message);
    // Error: not found: nonexistent
}
```

Common errors:
- `not found: {id}` — Document doesn't exist
- `index error for field '{field}': index not found` — Tried to drop a nonexistent index
- `I/O error at {path}: ...` — File system error
- `Failed to open database: ...` — Constructor failure
