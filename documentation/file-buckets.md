# nDB File Buckets

> Binary file storage alongside your documents — in two kinds.

---

## Two Kinds of Buckets

Every bucket is one of two kinds, declared per bucket in `meta.json`. **They are two keying strategies, not a hierarchy** — and they mix freely within one database:

| | `hash` (default) | `items` |
|---|---|---|
| identity | the content (SHA-256) | the item (`itm_…` id → folder) |
| dedup / refcounting | yes — the point | no — an item owns its folder |
| byte motion | `store` / `get` (whole buffers) | the app streams; nDB never holds a buffer |
| layout | `_files/<bucket>/<hash8>.<ext>` | `_files/<bucket>/<itemId>/` + `items.jsonl` |
| deletion | `releaseFile` / `gc_buckets` | folder → `_trash/`, whole |
| extra files per item | none | variants, description file — opaque payload, never parsed |
| built for | shared blobs (chat attachments, avatars) | app-managed assets (media pools) — any size |

- **Hash buckets** store each file once under its content hash. Identical content deduplicates; files are reference-counted from documents and garbage-collected when orphaned. The engine moves the bytes — `store` takes a buffer, `get` returns one.
- **Item buckets** group an item's files into one folder the application streams into itself. The engine hands out the folder path and keeps the records; it is never in the ingest path, so there is no buffer and no size ceiling. Choose this when a file *belongs* to something — an asset with variants and a description — rather than being shared content.

Undeclared buckets are `"hash"`, so existing databases upgrade without activating anything.

```
mydb/
├── data.jsonl                  # Document store (passed to Database::open)
├── meta.json                   # Declares kind (and policies) per bucket
└── _files/                     # All buckets, both kinds (created implicitly)
    ├── avatars/                # kind: "hash"
    │   └── a1b2c3d4.png        # Content-keyed blob
    └── media/                  # kind: "items"
        ├── items.jsonl         # Engine-owned record journal
        └── itm_V1StGXR8Z5jdHi6B/   # One folder per item
            ├── original.mp4    # Original (facts.name)
            ├── original_720.webp   # Variant — opaque payload
            └── asset.json      # Caller's description file
```

Trash mirrors the split: `_trash/files/<bucket>/` holds hash blobs by filename and item folders whole.

---

## Hash Buckets (`kind: "hash"`, the default)

Hash buckets provide named storage for binary data alongside your documents. Files are stored by their SHA-256 content hash, meaning identical content is stored only once (deduplication).

The sections from here to *FileMeta* — creating, storing, retrieving, deleting, listing, references — describe the hash kind. The item kind has its own section: *Item Buckets* further down.

## Creating a Bucket

Buckets are created implicitly when first used. Access a bucket through the database:

```rust
let bucket = db.bucket("avatars");
```

The bucket name becomes a subdirectory under `_files/`. Valid names: alphanumeric, hyphens, underscores.

### Bucket policies (`meta.json`)

Buckets may be *declared* in the database folder's `meta.json`, and the core enforces the policy block at open (#10):

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

The second kind from the table above (#9). Where a hash bucket keys blobs by content, an item bucket keys **folders** by an engine-issued item id — and the application, not the engine, moves the bytes. nDB is never in the ingest path, so there is no buffer and no size ceiling on this kind.

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

### The ingest pattern (worked example)

The engine is never in the byte path — reserve hands you a folder, you stream, you commit what you measured. End to end, Node.js:

```js
const { createWriteStream, createReadStream } = require('fs');
const { createHash } = require('crypto');
const { pipeline } = require('stream/promises');
const path = require('path');

// ── Ingest: an upload of any size (the spec's consumer takes 4 GiB) ──

// 1. Reserve — the engine creates the folder and hands out the path.
const { itemId, path: folder } = db.createItem('media');

try {
  // 2. Stream the upload to disk yourself, hashing and metering as you
  //    go. crypto.Hash is a Transform: bytes pass through unchanged and
  //    the digest accumulates — the hash costs you nothing extra.
  const hash = createHash('sha256');
  let size = 0;
  const meter = new (require('stream').Transform)({
    transform(chunk, _enc, cb) { size += chunk.length; cb(null, chunk); }
  });
  await pipeline(uploadStream, meter, hash, createWriteStream(path.join(folder, upload.filename)));

  //    Variants and the description file live beside the original —
  //    opaque payload the engine moves with the folder and never parses.
  await makeVariants(folder);                              // your code
  writeFileSync(path.join(folder, 'asset.json'), JSON.stringify(assetMeta));

  // 3. Commit — one write carrying the facts measured while streaming.
  db.commitItem('media', itemId, {
    name: upload.filename,       // required
    size,                        // required
    sha256: hash.digest('hex'),  // required
    mime: upload.mimetype,       // optional
    ...assetMeta                 // anything else passes through
  });
} catch (err) {
  // Aborted upload: never commit. An abandoned reservation is swept by
  // reserved_ttl_seconds — or delete it explicitly right now.
  db.deleteItem('media', itemId);
  throw err;
}

// ── Serve: reads are filesystem-direct too ──

const item = db.readItem('media', itemId);
res.setHeader('content-type', item.facts.mime);
res.setHeader('content-length', item.facts.size);
createReadStream(path.join(item.path, item.facts.name)).pipe(res);

// ── Housekeeping ──

db.deleteItem('media', itemId);          // folder → _trash/, whole, recoverable
db.restoreItem('media', itemId);         // ...and back
db.sweepReservedItems('media');          // abandoned reservations (also: TTL thread)
const anomalies = db.verifyItemBuckets(); // [] means every original matches its facts
```

The three things to internalize: **the path is engine-issued** (never hand-assemble it), **the hash is caller-computed** (you touched every byte anyway), and **an uncommitted item is not a failure** — it is a reserved item, and the sweep is its cleanup.

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
