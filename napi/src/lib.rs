//! ndb Node.js Native Bindings
//!
//! N-API bindings for the ndb document database,
//! enabling native-speed document operations from Node.js.
//!
//! Following the nVDB pattern: direct napi-rs per package, no shared bridge.

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::{Arc, RwLock};

use ndb::{Database as RustDatabase, Persistence, SortDir, QueryOptions};

// ─── Async Tasks ───────────────────────────────────────────────

pub struct CompactTask {
    db: Arc<RustDatabase>,
}

#[napi]
impl Task for CompactTask {
    type Output = ();
    type JsValue = ();
    fn compute(&mut self) -> Result<Self::Output> {
         self.db.compact().map_err(|e| Error::from_reason(format!("Compact failed: {}", e)))
    }
    fn resolve(&mut self, _env: Env, _output: Self::Output) -> Result<Self::JsValue> {
         Ok(())
    }
}

pub struct QueryTask {
    db: Arc<RustDatabase>,
    ast: serde_json::Value,
}

#[napi]
impl Task for QueryTask {
    type Output = Vec<serde_json::Value>;
    type JsValue = String;
    fn compute(&mut self) -> Result<Self::Output> {
        Ok(self.db.query(self.ast.clone()))
    }
    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        serde_json::to_string(&output).map_err(|e| Error::from_reason(format!("Serialization failed: {}", e)))
    }
}

pub struct QueryWithTask {
    db: Arc<RustDatabase>,
    ast: serde_json::Value,
    opts: QueryOptions,
}

#[napi]
impl Task for QueryWithTask {
    type Output = Vec<serde_json::Value>;
    type JsValue = String;
    fn compute(&mut self) -> Result<Self::Output> {
        Ok(self.db.query_with(self.ast.clone(), self.opts.clone()))
    }
    fn resolve(&mut self, _env: Env, output: Self::Output) -> Result<Self::JsValue> {
        serde_json::to_string(&output).map_err(|e| Error::from_reason(format!("Serialization failed: {}", e)))
    }
}

pub struct ExportTask {
    db: Arc<RustDatabase>,
    dest: std::path::PathBuf,
}

#[napi]
impl Task for ExportTask {
    type Output = ();
    type JsValue = ();
    fn compute(&mut self) -> Result<Self::Output> {
        self.db.export_snapshot(&self.dest).map_err(|e| Error::from_reason(format!("Export failed: {}", e)))
    }
    fn resolve(&mut self, _env: Env, _output: Self::Output) -> Result<Self::JsValue> {
        Ok(())
    }
}

// ─── Helper: JSON round-trip through napi ────────────────────────────
// napi-rs serde-json feature gives us serde_json::Value transfer,
// but we need to be careful with the type boundaries.

/// Database class for Node.js.
///
/// Wraps the Rust `Database` type and provides JS-friendly methods.
/// All documents are represented as plain JSON objects.
#[napi]
pub struct Database {
    inner: RwLock<Option<Arc<RustDatabase>>>,
}

impl Database {
    fn inner(&self) -> Result<Arc<RustDatabase>> {
        self.inner.read().unwrap().clone().ok_or_else(|| Error::from_reason("Database closed"))
    }
}

#[napi]
impl Database {
    /// Open or create a database at the given path.
    ///
    /// ```js
    /// const db = new Database('./my-data.jsonl');
    /// ```
    #[napi(constructor)]
    pub fn new(path: String) -> Result<Self> {
        let inner = RustDatabase::open(&path)
            .map_err(|e| Error::from_reason(format!("Failed to open database: {}", e)))?;
        Ok(Self {
            inner: RwLock::new(Some(Arc::new(inner))),
        })
    }

    /// Open or create a database with persistence options.
    ///
    /// ```js
    /// const db = Database.open('./my-data.jsonl', { persistence: 'immediate' });
    /// const db = Database.open('./my-data.jsonl', { persistence: 'scheduled', interval: 60 });
    /// ```
    #[napi]
    pub fn open(path: String, options: Option<DatabaseOptions>) -> Result<Self> {
        let mut db = RustDatabase::open(&path)
            .map_err(|e| Error::from_reason(format!("Failed to open database: {}", e)))?;

        if let Some(opts) = options {
            if let Some(ref mode) = opts.persistence {
                match mode.as_str() {
                    "immediate" | "Immediate" => {
                        db = db.with_persistence(Persistence::Immediate);
                    }
                    "scheduled" | "Scheduled" => {
                        let secs = opts.interval.unwrap_or(60) as u64;
                        db = db.with_persistence(Persistence::Scheduled(
                            std::time::Duration::from_secs(secs),
                        ));
                    }
                    "lazy" | "Lazy" | _ => {
                        db = db.with_persistence(Persistence::Lazy);
                    }
                }
            }
        }

        Ok(Self {
            inner: RwLock::new(Some(Arc::new(db))),
        })
    }

    /// Open an in-memory only database (no disk file).
    ///
    /// ```js
    /// const db = Database.openInMemory();
    /// ```
    #[napi]
    pub fn open_in_memory() -> Result<Self> {
        let inner = RustDatabase::open_in_memory()
            .map_err(|e| Error::from_reason(format!("Failed to create in-memory database: {}", e)))?;
        Ok(Self {
            inner: RwLock::new(Some(Arc::new(inner))),
        })
    }

    /// Close the database and instantly release any holds (OS locks, memory).
    /// Safe to call multiple times. Subsequent operations will throw "Database closed".
    #[napi]
    pub fn close(&self) -> Result<()> {
        *self.inner.write().unwrap() = None;
        Ok(())
    }

    // ─── Layer 1: Core Operations ──────────────────────────────────

    /// Insert a document. Returns the generated NanoID `_id`.
    ///
    /// ```js
    /// const id = db.insert({ title: 'Hello', tags: ['demo'] });
    /// ```
    #[napi]
    pub fn insert(&self, doc: String) -> Result<String> {
        let value: serde_json::Value = serde_json::from_str(&doc)
            .map_err(|e| Error::from_reason(format!("Invalid JSON document: {}", e)))?;
        self.inner()?.insert(value)
            .map_err(|e| Error::from_reason(format!("Insert failed: {}", e)))
    }

    /// Insert a document with a prefixed ID.
    ///
    /// ```js
    /// const id = db.insertWithPrefix('conv', { msg: 'hello' });
    /// // id → "conv_V1StGXR8Z5jdHi6B"
    /// ```
    #[napi]
    pub fn insert_with_prefix(&self, prefix: String, doc: String) -> Result<String> {
        let value: serde_json::Value = serde_json::from_str(&doc)
            .map_err(|e| Error::from_reason(format!("Invalid JSON document: {}", e)))?;
        self.inner()?.insert_with_prefix(&prefix, value)
            .map_err(|e| Error::from_reason(format!("Insert with prefix failed: {}", e)))
    }

    /// Get a document by ID. Returns the document as a JSON object.
    ///
    /// ```js
    /// const doc = db.get('V1StGXR8Z5jdHi6B');
    /// ```
    #[napi]
    pub fn get(&self, id: String) -> Result<String> {
        self.inner()?.get(&id)
            .map_err(|e| Error::from_reason(format!("Get failed: {}", e)))
            .and_then(|v| {
                serde_json::to_string(&v)
                    .map_err(|e| Error::from_reason(format!("Serialization failed: {}", e)))
            })
    }

    /// Update a document by ID. Replaces the entire document.
    ///
    /// ```js
    /// db.update('V1StGXR8Z5jdHi6B', { title: 'Updated' });
    /// ```
    #[napi]
    pub fn update(&self, id: String, doc: String) -> Result<()> {
        let value: serde_json::Value = serde_json::from_str(&doc)
            .map_err(|e| Error::from_reason(format!("Invalid JSON document: {}", e)))?;
        self.inner()?.update(&id, value)
            .map_err(|e| Error::from_reason(format!("Update failed: {}", e)))
    }

    /// Delete a document by ID (soft delete / tombstone).
    ///
    /// ```js
    /// db.delete('V1StGXR8Z5jdHi6B');
    /// ```
    #[napi]
    pub fn delete(&self, id: String) -> Result<()> {
        self.inner()?.delete(&id)
            .map_err(|e| Error::from_reason(format!("Delete failed: {}", e)))
    }

    // ─── Iteration & Counting ──────────────────────────────────────

    /// Get all documents as a JSON array string.
    ///
    /// ```js
    /// const docs = JSON.parse(db.iter());
    /// ```
    #[napi]
    pub fn iter(&self) -> Result<String> {
        let docs = self.inner()?.iter();
        serde_json::to_string(&docs)
            .map_err(|e| Error::from_reason(format!("Serialization failed: {}", e)))
    }

    /// Get document count.
    #[napi]
    pub fn len(&self) -> Result<u32> { Ok(self.inner()?.len() as u32) }

    /// Check if database is empty.
    #[napi]
    pub fn is_empty(&self) -> Result<bool> { Ok(self.inner()?.is_empty()) }

    /// Check if a document exists by ID.
    #[napi]
    pub fn contains(&self, id: String) -> Result<bool> { Ok(self.inner()?.contains(&id)) }

    // ─── Layer 2: Single Field Queries ─────────────────────────────

    /// Find documents where field equals value.
    /// Returns JSON array string.
    ///
    /// ```js
    /// const docs = JSON.parse(db.find('user_id', '"alice"'));
    /// const docs = JSON.parse(db.find('score', '42'));
    /// ```
    #[napi]
    pub fn find(&self, field: String, value: String) -> Result<String> {
        let val: serde_json::Value = serde_json::from_str(&value)
            .map_err(|e| Error::from_reason(format!("Invalid JSON value: {}", e)))?;
        let results = self.inner()?.find(&field, &val);
        serde_json::to_string(&results)
            .map_err(|e| Error::from_reason(format!("Serialization failed: {}", e)))
    }

    /// Find documents with field value in a range. Returns JSON array string.
    ///
    /// ```js
    /// const docs = JSON.parse(db.findRange('score', '10', '100'));
    /// ```
    #[napi]
    pub fn find_range(&self, field: String, min: String, max: String) -> Result<String> {
        let min_val: serde_json::Value = serde_json::from_str(&min)
            .map_err(|e| Error::from_reason(format!("Invalid JSON min value: {}", e)))?;
        let max_val: serde_json::Value = serde_json::from_str(&max)
            .map_err(|e| Error::from_reason(format!("Invalid JSON max value: {}", e)))?;
        let results = self.inner()?.find_range(&field, &min_val, &max_val);
        serde_json::to_string(&results)
            .map_err(|e| Error::from_reason(format!("Serialization failed: {}", e)))
    }

    // ─── Layer 3: JSON AST Queries ─────────────────────────────────

    /// Execute a JSON AST query limit. Returns JSON array string asynchronously.
    ///
    /// ```js
    /// const results = JSON.parse(await db.query({
    ///   "$and": [
    ///     { "user_id": { "$eq": "alice" } },
    ///     { "score": { "$gt": 100 } }
    ///   ]
    /// }));
    /// ```
    #[napi]
    pub fn query(&self, ast: String) -> Result<AsyncTask<QueryTask>> {
        let ast_value: serde_json::Value = serde_json::from_str(&ast)
            .map_err(|e| Error::from_reason(format!("Invalid JSON AST: {}", e)))?;
        Ok(AsyncTask::new(QueryTask { db: self.inner()?, ast: ast_value }))
    }

    /// Execute a JSON AST query with options (limit, offset, sort).
    /// Returns JSON array string.
    ///
    /// ```js
    /// const results = JSON.parse(db.queryWith(
    ///   '{"status":{"$eq":"active"}}',
    ///   { limit: 10, offset: 0, sortBy: 'created', sortDir: 'desc' }
    /// ));
    /// ```
    #[napi]
    pub fn query_with(
        &self,
        ast: String,
        limit: Option<u32>,
        offset: Option<u32>,
        sort_by: Option<String>,
        sort_dir: Option<String>,
    ) -> Result<AsyncTask<QueryWithTask>> {
        let ast_value: serde_json::Value = serde_json::from_str(&ast)
            .map_err(|e| Error::from_reason(format!("Invalid JSON AST: {}", e)))?;

        let dir = sort_dir
            .as_deref()
            .map(|d| match d {
                "desc" | "DESC" => SortDir::Desc,
                _ => SortDir::Asc,
            })
            .unwrap_or(SortDir::Asc);

        let opts = QueryOptions {
            limit: limit.map(|l| l as usize),
            offset: offset.map(|o| o as usize),
            sort_by: sort_by.map(|f| (f, dir)),
        };

        Ok(AsyncTask::new(QueryWithTask {
            db: self.inner()?,
            ast: ast_value,
            opts,
        }))
    }

    // ─── Index Management ──────────────────────────────────────────

    /// Create a hash index on a field for O(1) equality lookups.
    #[napi]
    pub fn create_index(&self, field: String) -> Result<()> {
        self.inner()?.create_index(&field)
            .map_err(|e| Error::from_reason(format!("Create index failed: {}", e)))
    }

    /// Create a BTree index on a field for range queries.
    #[napi]
    pub fn create_btree_index(&self, field: String) -> Result<()> {
        self.inner()?.create_btree_index(&field)
            .map_err(|e| Error::from_reason(format!("Create BTree index failed: {}", e)))
    }

    /// Drop an index, freeing memory.
    #[napi]
    pub fn drop_index(&self, field: String) -> Result<()> {
        self.inner()?.drop_index(&field)
            .map_err(|e| Error::from_reason(format!("Drop index failed: {}", e)))
    }

    /// Check if an index exists for a field.
    #[napi]
    pub fn has_index(&self, field: String) -> Result<bool> { Ok(self.inner()?.has_index(&field)) }

    // ─── Compaction & Trash ────────────────────────────────────────

    /// Compact the database asynchronously.
    #[napi]
    pub fn compact(&self) -> Result<AsyncTask<CompactTask>> {
        Ok(AsyncTask::new(CompactTask { db: self.inner()? }))
    }

    /// Flush data to disk.
    #[napi]
    pub fn flush(&self) -> Result<()> {
        self.inner()?.flush()
            .map_err(|e| Error::from_reason(format!("Flush failed: {}", e)))
    }

    /// Restore a deleted document from trash by ID.
    #[napi]
    pub fn restore(&self, id: String) -> Result<()> {
        self.inner()?.restore(&id)
            .map_err(|e| Error::from_reason(format!("Restore failed: {}", e)))
    }

    /// Get list of deleted document IDs.
    #[napi]
    pub fn deleted_ids(&self) -> Result<Vec<String>> { Ok(self.inner()?.deleted_ids()) }

    // ─── File Buckets ──────────────────────────────────────────────

    /// Store a file in a bucket. Returns file metadata as JSON string.
    ///
    /// ```js
    /// const meta = JSON.parse(db.storeFile('attachments', 'photo.png', imageBuffer, 'image/png'));
    /// ```
    #[napi]
    pub fn store_file(
        &self,
        bucket: String,
        name: String,
        data: Buffer,
        mime_type: String,
    ) -> Result<String> {
        let bkt = self.inner()?.bucket(&bucket);
        let meta = bkt
            .store(&name, &data, &mime_type)
            .map_err(|e| Error::from_reason(format!("Store file failed: {}", e)))?;
        serde_json::to_string(&meta)
            .map_err(|e| Error::from_reason(format!("Serialization failed: {}", e)))
    }

    /// Get a file from a bucket by hash and extension. Returns Buffer.
    ///
    /// ```js
    /// const data = db.getFile('attachments', 'a3f5c2d1', 'png');
    /// ```
    #[napi]
    pub fn get_file(&self, bucket: String, hash: String, ext: String) -> Result<Buffer> {
        let bkt = self.inner()?.bucket(&bucket);
        let data = bkt
            .get_by_hash(&hash, &ext)
            .map_err(|e| Error::from_reason(format!("Get file failed: {}", e)))?;
        Ok(Buffer::from(data))
    }

    /// Delete a file from a bucket (moves to trash).
    #[napi]
    pub fn delete_file(&self, bucket: String, hash: String, ext: String) -> Result<()> {
        let bkt = self.inner()?.bucket(&bucket);
        let file_ref = ndb::FileRef {
            bucket,
            id: hash,
            ext,
        };
        bkt.delete(&file_ref)
            .map_err(|e| Error::from_reason(format!("Delete file failed: {}", e)))
    }

    /// List files in a bucket.
    #[napi]
    pub fn list_files(&self, bucket: String) -> Result<Vec<String>> {
        let bkt = self.inner()?.bucket(&bucket);
        bkt.list()
            .map_err(|e| Error::from_reason(format!("List files failed: {}", e)))
    }
}

/// Database options for `Database.open()`.
#[napi(object)]
pub struct DatabaseOptions {
    /// Persistence mode: "lazy" (default), "immediate", or "scheduled".
    pub persistence: Option<String>,
    /// Interval in seconds for scheduled persistence. Default: 60.
    pub interval: Option<u32>,
}

