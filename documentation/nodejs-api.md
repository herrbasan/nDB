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

// Open a database
const db = new Database('./mydata.jsonl');

// Insert
const id = db.insert({ name: 'Alice', age: 30, email: 'alice@example.com' });
console.log('Inserted:', id);

// Get by ID
const doc = db.get(id);
console.log('Document:', doc);

// Update
db.update(id, { name: 'Alice Smith', age: 31, email: 'alice@example.com' });

// Query
const active = db.query({ status: { $eq: 'active' } });
console.log('Active users:', active.length);

// Delete
db.delete(id);
```

---

## `Database`

### Constructor

#### `new Database(path)`

Open or create a database at the given file path.

```js
const db = new Database('./data/app.jsonl');
```

#### `Database.open(path, options?)`

Open with configuration options.

```js
const db = Database.open('./data/app.jsonl', {
    persistence: 'immediate'
});

// Scheduled flush every 60 seconds
const db2 = Database.open('./data/app.jsonl', {
    persistence: 'scheduled',
    interval: 60
});
```

**Options:**

| Option | Type | Default | Description |
|--------|------|---------|-------------|
| `persistence` | `string` | `'lazy'` | `'lazy'`, `'immediate'`, or `'scheduled'` |
| `interval` | `number` | `60` | Seconds between flushes (for `scheduled` mode) |

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

### `get(id) → object | null`

Get a document by ID. Returns `null` if not found.

```js
const doc = db.get(id);
if (doc) {
    console.log(doc.name);
}
```

### `update(id, newDoc) → void`

Replace a document. The `_id` field is preserved.

```js
db.update(id, { name: 'Updated Name', age: 32 });
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

### `findWhere(field, predicate) → object[]`

Find documents where field value matches a predicate function.

```js
const seniors = db.findWhere('age', v => v >= 65);
const longNames = db.findWhere('name', v => v.length > 20);
```

### `findRange(field, min, max) → object[]`

Find documents with field value in range [min, max]. Works best with a BTree index.

```js
const midRange = db.findRange('score', 50, 100);
```

### `iter() → object[]`

Get all active documents.

```js
const allDocs = db.iter();
```

---

## Layer 3: JSON AST Queries

### `query(ast) → object[]`

Execute a JSON AST query. See [Query Language Reference](./query-language.md) for full syntax.

```js
// Simple equality
db.query({ status: 'active' });

// Comparison operators
db.query({ age: { $gte: 21 } });

// Combined conditions (implicit AND)
db.query({
    status: 'active',
    age: { $gte: 18 }
});

// Logical combinators
db.query({
    $or: [
        { role: 'admin' },
        { role: 'moderator' }
    ]
});

// Nested combinators
db.query({
    $and: [
        { $or: [{ status: 'active' }, { status: 'pending' }] },
        { age: { $gte: 18 } }
    ]
});
```

### `queryWith(ast, options) → object[]`

Query with limit, offset, and sort.

```js
const results = db.queryWith(
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

### `path() → string`

Get the database file path.

```js
console.log('DB path:', db.path());
```

---

## File Buckets

### `bucket(name) → FileBucket`

Get or create a named file bucket for binary storage.

```js
const avatars = db.bucket('avatars');
```

### FileBucket Methods

#### `store(name, data, mimeType) → FileMeta`

Store a file. Returns metadata.

```js
const fs = require('fs');
const imageData = fs.readFileSync('./photo.png');
const meta = avatars.store('photo.png', imageData, 'image/png');
console.log(meta);
// { _file: { bucket: 'avatars', id: 'a1b2c3d4', ext: 'png' },
//   name: 'photo.png', size: 12345, type: 'image/png', created: 1711553200 }
```

#### `get(fileRef) → Buffer`

Retrieve file content by reference.

```js
const data = avatars.get(meta._file);
fs.writeFileSync('./retrieved.png', data);
```

#### `delete(fileRef) → void`

Move a file to trash.

```js
avatars.delete(meta._file);
```

#### `restore(fileRef) → void`

Restore a file from trash.

```js
avatars.restore(meta._file);
```

#### `list() → FileMeta[]`

List all files in the bucket.

```js
const files = avatars.list();
files.forEach(f => console.log(f.name, f.size));
```

#### `purgeTrash() → void`

Permanently delete all trashed files.

```js
avatars.purgeTrash();
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
