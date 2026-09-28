/**
 * nDB Node.js Native Bindings
 *
 * This module loads the native nDB bindings and provides a high-level
 * JS API that handles JSON serialization transparently.
 *
 * Build the native module first with: cargo build --release -p ndb-node
 * Or run: node setup.js
 */

const { existsSync } = require('fs');
const { join, dirname } = require('path');

// Determine the correct native binary name based on platform
function getNativeBinaryName() {
  const platform = process.platform;
  const arch = process.arch;

  const names = {
    'win32': {
      'x64': 'ndb-node.win32-x64-msvc.node',
      'arm64': 'ndb-node.win32-arm64-msvc.node'
    },
    'darwin': {
      'x64': 'ndb-node.darwin-x64.node',
      'arm64': 'ndb-node.darwin-arm64.node'
    },
    'linux': {
      'x64': 'ndb-node.linux-x64-gnu.node',
      'arm64': 'ndb-node.linux-arm64-gnu.node'
    }
  };

  const platformNames = names[platform];
  if (!platformNames) {
    throw new Error(`Unsupported platform: ${platform}`);
  }

  const binaryName = platformNames[arch];
  if (!binaryName) {
    throw new Error(`Unsupported architecture ${arch} on ${platform}`);
  }

  return binaryName;
}

// Find the native binary
function findNativeBinary() {
  const binaryName = getNativeBinaryName();
  const moduleDir = __dirname;

  const searchPaths = [
    // 1. Same directory as this file (prebuilt)
    join(moduleDir, binaryName),
    // 2. Raw DLL name (Windows dev builds)
    join(moduleDir, 'ndb_node.dll'),
    // 3. Parent directory (target/release relative to napi folder)
    join(moduleDir, '..', 'target', 'release', 'ndb_node.dll'),
    join(moduleDir, '..', 'target', 'release', 'libndb_node.so'),
    join(moduleDir, '..', 'target', 'release', 'libndb_node.dylib'),
    // 4. Debug builds
    join(moduleDir, '..', 'target', 'debug', 'ndb_node.dll'),
    join(moduleDir, '..', 'target', 'debug', 'libndb_node.so'),
    join(moduleDir, '..', 'target', 'debug', 'libndb_node.dylib'),
    // 5. Direct build output (various platforms)
    join(moduleDir, 'ndb_node.node'),
    join(moduleDir, 'ndb_node.dll'),
    join(moduleDir, 'libndb_node.so'),
    join(moduleDir, 'libndb_node.dylib'),
  ];

  for (const path of searchPaths) {
    if (existsSync(path)) {
      return path;
    }
  }

  throw new Error(
    `Native binary not found. The native module must be built after cloning.\n\n` +
    `Searched:\n` +
    searchPaths.map(p => `  - ${p}`).join('\n') +
    `\n\nTo build, run:\n` +
    `  cd ndb/napi && node setup.js\n` +
    `\nOr manually:\n` +
    `  cargo build --release -p ndb-node\n` +
    `\nYou can also set the environment variable:\n` +
    `  NODE_NDB_NATIVE_PATH=/path/to/native/binary`
  );
}

// Allow override via environment variable
const nativePath = process.env.NODE_NDB_NATIVE_PATH || findNativeBinary();

// Load the native module
let nativeBinding;
try {
  nativeBinding = require(nativePath);
} catch (e) {
  throw new Error(`Failed to load native module from ${nativePath}: ${e.message}`);
}

// ─── High-Level JS Wrapper ──────────────────────────────────────────
// The native module works with JSON strings for documents.
// This wrapper provides the ergonomic JS API that auto-serializes.

/**
 * nDB Database - Human-readable document database.
 *
 * ```js
 * const { Database } = require('ndb');
 * const db = Database.open('./my-data');
 * const id = db.insert({ title: 'Hello World' });
 * ```
 */
class Database {
  constructor(path) {
    this._native = new nativeBinding.Database(path);
  }

  /**
   * Open or create a database with optional persistence config.
   * @param {string} path - Path to the database file.
   * @param {object} [options] - Persistence options.
   * @param {string} [options.persistence] - "lazy" | "immediate" | "scheduled"
   * @param {number} [options.interval] - Seconds between flushes (scheduled mode).
   * @param {number} [options.trash_ttl] - Auto-empty trash TTL in seconds. Default: no auto-empty.
   * @param {number} [options.trash_purge_interval] - Background interval in seconds to check for expired trash. Default: 3600 (1 hour).
   * @returns {Database}
   */
  static open(path, options) {
    // Never build a throwaway binding: `new Database(path)` would open a second
    // handle that only the GC releases, and on Windows that handle keeps the
    // folder locked even after close().
    const db = Object.create(Database.prototype);
    db._native = options
      ? nativeBinding.Database.open(path, options)
      : new nativeBinding.Database(path);
    return db;
  }

  /**
   * Open an in-memory only database.
   * @returns {Database}
   */
  static openInMemory() {
    const db = Object.create(Database.prototype);
    db._native = nativeBinding.Database.openInMemory();
    return db;
  }

  /**
   * Insert a document. Returns the generated NanoID.
   * @param {object} doc - Document to insert.
   * @returns {string} Generated _id.
   */
  insert(doc) {
    return this._native.insert(JSON.stringify(doc));
  }

  /**
   * Insert a document with a prefixed ID.
   * @param {string} prefix - ID prefix (e.g., "conv").
   * @param {object} doc - Document to insert.
   * @returns {string} Generated prefixed _id.
   */
  insertWithPrefix(prefix, doc) {
    return this._native.insertWithPrefix(prefix, JSON.stringify(doc));
  }

  /**
   * Get a document by ID.
   * @param {string} id - Document ID.
   * @returns {object|null} The document, or throws if not found.
   */
  get(id) {
    const json = this._native.get(id);
    return JSON.parse(json);
  }

  /**
   * Update a document by ID (full replacement).
   * @param {string} id - Document ID.
   * @param {object} doc - New document content.
   */
  update(id, doc) {
    this._native.update(id, JSON.stringify(doc));
  }

  /**
   * Delete a document by ID (soft delete).
   *
   * Throws when there is no such document, and when the deletion could not be
   * recorded — an unrecorded delete is refused, because the trash copy is what
   * makes it reversible.
   * @param {string} id - Document ID.
   */
  delete(id) {
    this._native.delete(id);
  }

  /**
   * Append a value to an array field.
   * @param {string} id - Document ID.
   * @param {string} field - Array field name (dot-separated path allowed).
   * @param {*} value - Value to append.
   * @returns {boolean} true when the push landed; false when the path did
   *   not resolve (no-op — nothing changed, nothing journaled).
   */
  arrayPush(id, field, value) {
    return this._native.arrayPush(id, field, JSON.stringify(value));
  }

  /**
   * Set a value at a dot-separated path within a document.
   * @param {string} id - Document ID.
   * @param {string} path - Dot-separated path (e.g. "messages.3.text").
   * @param {*} value - Value to set.
   * @returns {boolean} true when the assignment landed (including a
   *   same-value re-assignment); false when the path did not resolve
   *   (no-op — nothing changed, nothing journaled).
   */
  set(id, path, value) {
    return this._native.set(id, path, JSON.stringify(value));
  }

  /**
   * Remove a field or array element at a dot-separated path.
   * @param {string} id - Document ID.
   * @param {string} path - Dot-separated path (e.g. "messages.3" or "settings.theme").
   * @returns {boolean} true when something was removed; false when the path
   *   did not resolve (no-op — nothing changed, nothing journaled).
   */
  remove(id, path) {
    return this._native.remove(id, path);
  }

  /**
   * Get all documents.
   * @returns {object[]}
   */
  iter() {
    return JSON.parse(this._native.iter());
  }

  /**
   * Get document count.
   * @returns {number}
   */
  len() {
    return this._native.len();
  }

  /**
   * Check if database is empty.
   * @returns {boolean}
   */
  isEmpty() {
    return this._native.isEmpty();
  }

  /**
   * Check if a document exists.
   * @param {string} id - Document ID.
   * @returns {boolean}
   */
  contains(id) {
    return this._native.contains(id);
  }

  /**
   * Find documents where field equals value.
   * @param {string} field - Field name.
   * @param {*} value - Value to match.
   * @returns {object[]}
   */
  find(field, value) {
    return JSON.parse(this._native.find(field, JSON.stringify(value)));
  }

  /**
   * Find documents with field value in a range.
   * @param {string} field - Field name.
   * @param {*} min - Minimum value (inclusive).
   * @param {*} max - Maximum value (inclusive).
   * @returns {object[]}
   */
  findRange(field, min, max) {
    return JSON.parse(this._native.findRange(field, JSON.stringify(min), JSON.stringify(max)));
  }

  /**
   * Execute a JSON AST query.
   * @param {object} ast - Query AST.
   * @returns {object[]}
   */
  async query(ast) {
    return JSON.parse(await this._native.query(JSON.stringify(ast)));
  }

  /**
   * Execute a JSON AST query with options.
   * @param {object} ast - Query AST.
   * @param {object} [options] - Query options.
   * @param {number} [options.limit] - Max results.
   * @param {number} [options.offset] - Skip first N results.
   * @param {string} [options.sortBy] - Field to sort by.
   * @param {string} [options.sortDir] - "asc" or "desc".
   * @returns {object[]}
   */
  async queryWith(ast, options) {
    const opts = options || {};
    return JSON.parse(await this._native.queryWith(
      JSON.stringify(ast),
      opts.limit,
      opts.offset,
      opts.sortBy,
      opts.sortDir
    ));
  }

  /**
   * Projected, paginated query — the compact-list shape. Filters, sorts
   * and paginates on the Rust side; returns ONLY the named fields (plus
   * `_id`) and the pre-pagination `total`. One boundary crossing per page;
   * unprojected fields never cross. Omit `fields` for full documents.
   * @param {object} ast - Query AST.
   * @param {object} [options] - limit / offset / sortBy / sortDir.
   * @param {string[]} [fields] - Field names to return.
   * @returns {{total: number, results: object[]}}
   */
  async queryPage(ast, options, fields) {
    const opts = options || {};
    return JSON.parse(await this._native.queryPage(
      JSON.stringify(ast),
      opts.limit,
      opts.offset,
      opts.sortBy,
      opts.sortDir,
      fields
    ));
  }

  /**
   * Create a hash index on a field.
   * @param {string} field - Field name.
   */
  createIndex(field) {
    this._native.createIndex(field);
  }

  /**
   * Create a BTree index on a field (for range queries).
   * @param {string} field - Field name.
   */
  createBTreeIndex(field) {
    this._native.createBtreeIndex(field);
  }

  /**
   * Drop an index.
   * @param {string} field - Field name.
   */
  dropIndex(field) {
    this._native.dropIndex(field);
  }

  /**
   * Check if an index exists.
   * @param {string} field - Field name.
   * @returns {boolean}
   */
  hasIndex(field) {
    return this._native.hasIndex(field);
  }

  /**
   * Create a full-text index on a field (opt-in, like hash/btree).
   * Tokenizes existing documents once; maintained on every write path.
   * Idempotent — safe to call on every open.
   * @param {string} field - Field name (string or array-of-strings values).
   */
  createTextIndex(field) {
    this._native.createTextIndex(field);
  }

  /**
   * Drop a full-text index (and its disk cache, if any).
   * @param {string} field - Field name.
   */
  dropTextIndex(field) {
    this._native.dropTextIndex(field);
  }

  /**
   * Full-text search over an indexed field. Fails loud when the field has
   * no text index.
   * @param {string} field - Indexed field name.
   * @param {object} search - {mode: 'and'|'or', case_sensitive: false, queries: [{type: 'term'|'phrase'|'prefix', value, exclude?}]}
   * @returns {string[]} Matching _ids (unordered).
   */
  textSearch(field, search) {
    return JSON.parse(this._native.textSearch(field, JSON.stringify(search)));
  }

  /**
   * Compact the database.
   */
  async compact() {
    await this._native.compact();
  }

  /**
   * Flush data to disk.
   */
  flush() {
    this._native.flush();
  }

  /**
   * Close the database and release its file handles.
   *
   * Safe to call more than once. Any later operation reports "Database closed".
   * Call this before renaming or deleting the database folder on Windows —
   * while a handle is open, the folder cannot be moved.
   *
   * Await any pending `query`, `queryWith` or `compact` first: those hold their
   * own reference to the database, so close() alone will not release the folder
   * while one is still running.
   */
  close() {
    this._native.close();
  }

  /**
   * Restore a deleted document.
   * @param {string} id - Document ID.
   */
  restore(id) {
    this._native.restore(id);
  }

  /**
   * Get list of deleted document IDs.
   * @returns {string[]}
   */
  deletedIds() {
    return this._native.deletedIds();
  }

  /**
   * Store a file in a bucket.
   * @param {string} bucket - Bucket name.
   * @param {string} name - Original filename.
   * @param {Buffer} data - File content.
   * @param {string} mimeType - MIME type.
   * @returns {object} File metadata.
   */
  storeFile(bucket, name, data, mimeType) {
    return JSON.parse(this._native.storeFile(bucket, name, data, mimeType));
  }

  /**
   * Get a file from a bucket.
   * @param {string} bucket - Bucket name.
   * @param {string} hash - File hash.
   * @param {string} ext - File extension.
   * @returns {Buffer}
   */
  getFile(bucket, hash, ext) {
    return this._native.getFile(bucket, hash, ext);
  }

  /**
   * Restore a file from bucket trash to the active bucket.
   * @param {string} bucket
   * @param {string} hash
   * @param {string} ext
   * @returns {boolean} true if restored from trash, false if not in trash
   */
  restoreFile(bucket, hash, ext) {
    return this._native.restoreFile(bucket, hash, ext);
  }

  /**
   * Safe garbage collecting release of a file from a bucket.
   * If there are no active document references, the file is trashed.
   * @param {string} fileRef - File reference id (e.g. images:b1a2c3.png)
   * @returns {boolean} Whether the file was trashed.
   */
  releaseFile(fileRef) {
    return this._native.releaseFile(fileRef);
  }

  /**
   * Delete a file from a bucket.
   * @param {string} bucket - Bucket name.
   * @param {string} hash - File hash.
   * @param {string} ext - File extension.
   */
  deleteFile(bucket, hash, ext) {
    this._native.deleteFile(bucket, hash, ext);
  }

  /**
   * List files in a bucket.
   * @param {string} bucket - Bucket name.
   * @returns {string[]}
   */
  listFiles(bucket) {
    return this._native.listFiles(bucket);
  }

  /**
   * Run garbage collection on all file buckets.
   * Scans all file buckets and trashes files that are no longer referenced.
   * @returns {number} The count of files moved to trash.
   */
  gcBuckets() {
    return this._native.gcBuckets();
  }

  // ─── Item Buckets (kind: "items" in meta.json) ─────────────────────
  //
  // The engine hands out a folder path; the application streams bytes to
  // it itself — nDB is never in the ingest path and never holds a buffer.

  /**
   * Reserve an item. The engine creates `_files/<bucket>/<itemId>/` and
   * returns the id and folder path. Stream bytes to that path yourself,
   * then call commitItem.
   * @param {string} bucket - A bucket declared `kind: "items"` in meta.json.
   * @returns {{ itemId: string, path: string }}
   */
  createItem(bucket) {
    return JSON.parse(this._native.createItem(bucket));
  }

  /**
   * Commit a reserved item. One write carrying the facts; `name`, `size`
   * and `sha256` (computed by you while streaming) are required, `mime`
   * and any other fields pass through.
   * @param {string} bucket
   * @param {string} itemId
   * @param {object} facts - { name, size, sha256, mime?, ...custom }
   */
  commitItem(bucket, itemId, facts) {
    this._native.commitItem(bucket, itemId, JSON.stringify(facts));
  }

  /**
   * Read one item. Throws when unknown or deleted.
   * @returns {{ itemId: string, state: 'reserved'|'live', facts: object|null,
   *   created: number, committed: number|null, path: string }}
   */
  readItem(bucket, itemId) {
    return JSON.parse(this._native.readItem(bucket, itemId));
  }

  /**
   * List items of a bucket, optionally filtered by state.
   * @param {string} bucket
   * @param {'reserved'|'live'} [state]
   * @returns {Array} Items in the readItem shape.
   */
  listItems(bucket, state) {
    return JSON.parse(this._native.listItems(bucket, state ?? null));
  }

  /**
   * Delete an item: record tombstoned, folder moved to trash.
   */
  deleteItem(bucket, itemId) {
    this._native.deleteItem(bucket, itemId);
  }

  /**
   * Restore a tombstoned item: tombstone lifted, folder moved back.
   */
  restoreItem(bucket, itemId) {
    this._native.restoreItem(bucket, itemId);
  }

  /**
   * Sweep reserved items older than the bucket's reserved_ttl_seconds
   * (default 30 min): tombstoned and trashed, exactly like deleteItem.
   * @returns {number} The number of items swept.
   */
  sweepReservedItems(bucket) {
    return this._native.sweepReservedItems(bucket);
  }

  /**
   * Verify all item buckets: live originals match committed size/sha256,
   * reserved items have their folder, orphan folders are flagged.
   * @returns {string[]} Anomaly descriptions; empty means clean.
   */
  verifyItemBuckets() {
    return JSON.parse(this._native.verifyItemBuckets());
  }
}

// ─── Exports ─────────────────────────────────────────────────────────

module.exports.Database = Database;
module.exports.NATIVE_PATH = nativePath;
