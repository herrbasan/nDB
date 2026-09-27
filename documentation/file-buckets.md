# nDB File Buckets

> Binary file storage with SHA-256 content-hash deduplication.

---

## Overview

File Buckets provide named storage for binary data alongside your documents. Files are stored by their SHA-256 content hash, meaning identical content is stored only once (deduplication).

```
mydb/
├── data.jsonl                   # Document store (passed to Database::open)
└── _files/                       # All file buckets (created implicitly, sibling of data.jsonl)
    ├── avatars/                  # Bucket "avatars"
    │   ├── a1b2c3d4e5f6.png      # Stored by hash prefix
    │   └── g7h8i9j0k1l2.jpg
    └── attachments/              # Bucket "attachments"
        └── m3n4o5p6q7r8.pdf
```

---

## Creating a Bucket

Buckets are created implicitly when first used. Access a bucket through the database:

```rust
let bucket = db.bucket("avatars");
```

The bucket name becomes a subdirectory under `_files/`. Valid names: alphanumeric, hyphens, underscores.

### Bucket policies (`meta.json`)

Buckets may be *declared* in the database folder's `meta.json`, and the core enforces two policy keys at open (#10):

```json
{
  "buckets": {
    "avatars":     { "onDocumentDelete": "restrict" },
    "attachments": { "onDocumentDelete": "trash", "ttl_seconds": 2592000 },
    "temp_exports": { "ttl_seconds": 86400 }
  }
}
```

- **`onDocumentDelete: "restrict"`** — deleting a document that references a file in this bucket **fails** (`PolicyViolation`) before anything moves. The caller must reassign or explicitly release the file first.
- **`onDocumentDelete: "trash"`** — the explicit form of the default: the document's refs are released and orphaned files move to trash (refcount protection unchanged).
- **`ttl_seconds`** — per-bucket trash TTL, used by `purge_trash()` and the background TTL sweep *instead of* the database-wide TTL for this bucket's trash. Independent of `onDocumentDelete`.
- **`kind`** — `"hash"` (default) or `"items"`; see *Item Buckets* below.
- **`reserved_ttl_seconds`** — reserved-item TTL for `kind: "items"` buckets (default 1800).

Absence — no `meta.json`, no `buckets` block, no entry for the bucket — means exactly the default behavior. A **malformed** policy (unknown `onDocumentDelete` value, non-object block, non-numeric `ttl_seconds`, invalid JSON) fails `open()` with a corruption error: a config typo must never silently degrade to no-policy. Unknown top-level keys (`schemas`, future fields) are ignored. The file is read at open; changes require a reopen.

---

## Item Buckets (`kind: "items"`)

A second bucket kind for app-managed assets (#9). Where a hash bucket keys blobs by content, an item bucket keys **folders** by an engine-issued item id — and the application, not the engine, moves the bytes. nDB is never in the ingest path, so there is no buffer and no size ceiling on this kind.

| | `hash` (default) | `items` |
|---|---|---|
| identity | the content (SHA-256) | the item (`itm_…` id → folder) |
| dedup / refcounting | yes — the point | no — an item owns its folder |
| byte motion | `store` / `get` (whole buffers) | the app streams; nDB never holds a buffer |
| layout | `_files/<bucket>/<hash8>.<ext>` | `_files/<bucket>/<itemId>/` + `items.jsonl` |
| deletion | `releaseFile` / `gc_buckets` | folder → `_trash/`, whole |
| extra files per item | none | variants, description file — opaque payload, never parsed |

### Declaration

```json
{ "buckets": { "media": { "kind": "items", "reserved_ttl_seconds": 1800 } } }
```

Absence of `kind` means `"hash"` — existing databases activate nothing. Records live in an append-only journal at `_files/<bucket>/items.jsonl` (full-record lines, last write wins, tombstones mark deletions), replayed at open.

### Lifecycle

1. **`createItem(bucket)` → `{ itemId, path }`**, state `reserved`. The engine creates the folder; the application streams bytes to `path` itself and may add variant/description files.
2. **`commitItem(itemId, facts)`** — one write. `name`, `size`, `sha256` are required (the caller computes the hash while streaming — it touched every byte); `mime` and any other fields pass through. State `live`. Only a reserved item can be committed.
3. **`readItem` / `listItems(bucket, state?)`** — record + engine-issued path. Tombstoned items are never listed.
4. **`deleteItem(itemId)`** — record tombstoned, folder moved to `_trash/files/<bucket>/<itemId>/` whole. **`restoreItem(itemId)`** reverses it.
5. **Reserved sweep** — reserved items older than `reserved_ttl_seconds` (default 30 min) are tombstoned and trashed by `sweepReservedItems()` and by the TTL background thread. An abandoned reservation is never an undeletable empty folder.

### Integrity (`verify`)

`ndb verify` and `verifyItemBuckets()` check, per items bucket: every **live** item's original (`facts.name`) exists and matches its committed `size`/`sha256` (hashed streaming — originals are never read whole); every **reserved** item has its folder; every folder has a live record, else it is flagged an orphan (sweepable).

The crash windows are the design: folder-without-record → orphan, sweepable; record-without-commit → reserved, TTL-swept; bytes-differ-from-facts → verify flags; folder-fails-to-move on delete → tombstone stands, folder stays, verify flags it (under-cleaning, never silent).

### Non-goals

No streaming store/get API in the engine (the application owns byte motion, by design), no engine knowledge of variants, no dedup or refcounting on the item kind, no cross-database sharing. The hash kind is byte- and semantics-identical to before.

---

## Storing Files

### `store(name: &str, data: &[u8], mime_type: &str) -> Result<FileMeta>`

Store a file. Returns metadata including the content hash reference.

```rust
let data = std::fs::read("photo.png")?;
let meta = bucket.store("photo.png", &data, "image/png")?;

// meta = FileMeta {
//     _file: FileRef {
//         bucket: "avatars",
//         id: "a1b2c3d4",      // First 8 chars of SHA-256
//         ext: "png",
//     },
//     name: "photo.png",
//     size: 45678,
//     type_: "image/png",
//     created: 1711553200,
// }
```

### Deduplication

If you store the same file content twice, the second call returns metadata pointing to the same stored file. No duplicate data is written.

```rust
let meta1 = bucket.store("copy1.png", &data, "image/png")?;
let meta2 = bucket.store("copy2.png", &data, "image/png")?;
// meta1._file.id == meta2._file.id  (same hash, same stored file)
```

---

## Retrieving Files

### `get(file_ref: &FileRef) -> Result<Vec<u8>>`

Retrieve file content by its reference.

```rust
let data = bucket.get(&meta._file)?;
std::fs::write("retrieved.png", &data)?;
```

### `get_by_id(hash: &str, ext: &str) -> Result<Vec<u8>>`

Retrieve by hash and extension directly.

```rust
let data = bucket.get_by_id("a1b2c3d4", "png")?;
```

---

## Deleting Files

### `delete(file_ref: &FileRef) -> Result<()>`

Move a file to the bucket's trash directory.

```rust
bucket.delete(&meta._file)?;
```

Files are **not** permanently deleted immediately. They are moved to:
```
_trash/files/{bucket_name}/{hash}.{ext}
```

### `restore(hash: &str, ext: &str) -> Result<()>`

Restore a file from trash.

```rust
bucket.restore(&meta._file.id, &meta._file.ext)?;
```

### Automatic Garbage Collection

In `nDB`, file trashing is designed to happen proactively via `gc_buckets()`. Due to atomic ref-counting on active paths embedded within JSON docs, calling `db.gc_buckets()` parses all dynamically unreferenced files and sweeps them entirely out of the active buckets into the `_trash/` directories in O(n_files) time.

```rust
let trashed = db.gc_buckets()?;
println!("GC collected {} orphaned files.", trashed);
```

The returned count is files that actually moved. A bucket that cannot be enumerated, and a file that cannot be moved, are reported on stderr (`ndb: suppressed failure: ...`) and left out of the count — so calling this a successful sweep of *n* files is always true of *n* files. Nothing here throws: a sweep you asked for reports how far it got.

### `purge_trash_ttl(ttl: Duration) -> Result<()>`

Delete all trashed files in this bucket that exceed the given TTL by reading their filesystem modification date.

```rust
bucket.purge_trash_ttl(Duration::from_secs(86400))?;
```

---

## Listing Files

### `list() -> Result<Vec<String>>`

List all active files in the bucket. Returns the stored filenames (e.g. `a1b2c3d4e5f6.png`). It reads the directory directly — there are no companion `.meta` files.

```rust
let files = bucket.list()?;
for file in &files {
    println!("{} ({} bytes, {})", file.name, file.size, file.type_);
}
```

---

## Storing File References in Documents

The `FileMeta` struct is designed to be embedded in documents. Store the `FileRef` in your document to link it to a file:

```rust
let meta = bucket.store("report.pdf", &pdf_data, "application/pdf")?;

let doc_id = db.insert(json!({
    "title": "Q4 Report",
    "file": {
        "bucket": meta._file.bucket,
        "id": meta._file.id,
        "ext": meta._file.ext,
        "name": meta.name,
        "size": meta.size,
        "type": meta.type_,
        "created": meta.created
    }
}))?;
```

Or use the compact string form:

```rust
let doc_id = db.insert(json!({
    "title": "Q4 Report",
    "_file": meta._file.to_string_compact()
    // "avatars:a1b2c3d4.png"
}))?;
```

---

## FileRef

Reference to a stored file.

| Field | Type | Description |
|-------|------|-------------|
| `bucket` | `String` | Bucket name |
| `id` | `String` | First 8 chars of SHA-256 hash |
| `ext` | `String` | File extension (without dot) |

### Methods

- `filename()` → `"{id}.{ext}"` e.g. `"a1b2c3d4.png"`
- `to_string_compact()` → `"{bucket}:{id}.{ext}"` e.g. `"avatars:a1b2c3d4.png"`
- `from_compact(s)` → Parse from compact string

## FileMeta

Full file metadata.

| Field | Type | Description |
|-------|------|-------------|
| `_file` | `FileRef` | File reference |
| `name` | `String` | Original filename |
| `size` | `usize` | Size in bytes |
| `type_` | `String` | MIME type |
| `created` | `u64` | UNIX timestamp |

---

## SHA-256 Implementation

nDB implements SHA-256 internally without external cryptographic crates. The hash is computed over the file content bytes, producing a 64-character hex string. The first 8 characters are used as the storage filename.

This provides:
- **Content-addressed storage**: same content → same hash → same file
- **Integrity verification**: re-hash on read to verify content
- **No collisions**: SHA-256 collision resistance is computationally infeasible
