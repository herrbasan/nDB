# nDB File Buckets

> Binary file storage with SHA-256 content-hash deduplication.

---

## Overview

File Buckets provide named storage for binary data alongside your documents. Files are stored by their SHA-256 content hash, meaning identical content is stored only once (deduplication).

```
mydb/
├── mydb.jsonl                    # Document store
└── _files/                       # All file buckets
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

### `purge_trash_ttl(ttl: Duration) -> Result<()>`

Delete all trashed files in this bucket that exceed the given TTL by reading their filesystem modification date.

```rust
bucket.purge_trash_ttl(Duration::from_secs(86400))?;
```

---

## Listing Files

### `list() -> Result<Vec<FileMeta>>`

List all active files in the bucket. Reads metadata from each file's companion `.meta` JSON file.

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
