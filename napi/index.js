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
   * @returns {Database}
   */
  static open(path, options) {
    const db = new Database(path);
    // If options provided, reopen with options via native open
    if (options) {
      db._native = nativeBinding.Database.open(path, options);
    }
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
   * @param {string} id - Document ID.
   */
  delete(id) {
    this._native.delete(id);
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
}

// ─── Exports ─────────────────────────────────────────────────────────

module.exports.Database = Database;
module.exports.NATIVE_PATH = nativePath;
