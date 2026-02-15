//! nDB - High-performance embedded vector database
//!
//! nDB is an embedded, in-memory vector database designed for LLM workflows.
//! It prioritizes reliability and performance over feature breadth.
//!
//! # Core Philosophy
//!
//! - **Deterministic correctness**: Design failures away rather than handling them
//! - **Zero-cost abstractions**: Pay only for what you use
//! - **Instant recovery**: Memory-mapped persistence means large datasets don't require loading
//! - **Read-heavy optimization**: Readers never block; writers use append-only patterns
//!
//! # Example
//!
//! ```no_run
//! use ndb::{Database, CollectionConfig, Durability, Document};
//!
//! # fn main() -> Result<(), Box<dyn std::error::Error>> {
//! let db = Database::open("data/")?;
//! let collection = db.create_collection("embeddings", CollectionConfig {
//!     dim: 768,
//!     durability: Durability::FdatasyncEachBatch,
//! })?;
//!
//! // Insert a document
//! collection.insert(Document {
//!     id: "doc1".to_string(),
//!     vector: vec![0.1; 768],
//!     payload: Some(serde_json::json!({"title": "Example"})),
//! })?;
//!
//! // Flush to segment
//! collection.flush()?;
//! # Ok(())
//! # }
//! ```

#![warn(missing_docs)]
#![warn(rust_2018_idioms)]

pub mod compaction;
pub mod distance;
pub mod error;
pub mod filter;
pub mod hnsw;
pub mod id;
pub mod lock;
pub mod manifest;
pub mod memtable;
pub mod search;
pub mod segment;
pub mod wal;

pub use compaction::CompactionResult;
pub use distance::Distance;
pub use error::{Error, Result};
pub use filter::Filter;
pub use hnsw::{HnswIndex, HnswParams};
pub use id::IdMapping;
pub use lock::CollectionLock;
pub use manifest::{Manifest, SegmentEntry};
pub use memtable::Memtable;
pub use search::{Match, Search, exact_search};
pub use segment::{Document, Segment, SegmentBuilder, SegmentHeader};
pub use wal::{Wal, WAL_FLUSH_THRESHOLD};

use arc_swap::ArcSwap;
use serde::{Deserialize, Serialize};

use std::cmp::Ordering;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, RwLock};

/// Durability level for write operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum Durability {
    /// Acknowledge after append to OS page cache.
    /// Fastest, but data loss window is ~5-30 seconds (OS dependent).
    Buffered,

    /// Acknowledge after fdatasync() completes.
    /// Data is on disk before returning. Slower, but no data loss on crash.
    FdatasyncEachBatch,
}

impl Default for Durability {
    fn default() -> Self {
        Durability::Buffered
    }
}

/// Configuration for a collection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectionConfig {
    /// Vector dimension for this collection.
    pub dim: usize,
    /// Durability level for writes.
    #[serde(default)]
    pub durability: Durability,
}

impl CollectionConfig {
    /// Create a new config with the given dimension and default durability.
    pub fn new(dim: usize) -> Self {
        Self {
            dim,
            durability: Durability::default(),
        }
    }

    /// Set the durability level.
    pub fn with_durability(mut self, durability: Durability) -> Self {
        self.durability = durability;
        self
    }
}

/// Database-level manifest tracking all collections.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct DatabaseManifest {
    /// Database format version
    version: u32,
    /// Collection names
    collections: Vec<String>,
}

impl DatabaseManifest {
    const CURRENT_VERSION: u32 = 1;
    const FILENAME: &'static str = "MANIFEST";

    fn new() -> Self {
        Self {
            version: Self::CURRENT_VERSION,
            collections: Vec::new(),
        }
    }

    fn load(path: &Path) -> Result<Option<Self>> {
        let manifest_path = path.join(Self::FILENAME);
        if !manifest_path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&manifest_path)
            .map_err(Error::io_err(&manifest_path, "failed to read database manifest"))?;

        let manifest: DatabaseManifest = serde_json::from_str(&contents)
            .map_err(|e| Error::corruption(&manifest_path, 0, format!("invalid JSON: {}", e)))?;

        if manifest.version != Self::CURRENT_VERSION {
            return Err(Error::corruption(
                &manifest_path,
                0,
                format!(
                    "unsupported version: expected {}, got {}",
                    Self::CURRENT_VERSION,
                    manifest.version
                ),
            ));
        }

        Ok(Some(manifest))
    }

    fn save(&self, path: &Path) -> Result<()> {
        let manifest_path = path.join(Self::FILENAME);
        let temp_path = manifest_path.with_extension("tmp");

        let json = serde_json::to_vec_pretty(self)
            .map_err(|e| Error::Serialization(format!("failed to serialize: {}", e)))?;

        fs::write(&temp_path, json)
            .map_err(Error::io_err(&temp_path, "failed to write temp manifest"))?;

        fs::rename(&temp_path, &manifest_path)
            .map_err(Error::io_err(&manifest_path, "failed to rename manifest"))?;

        Ok(())
    }

    fn add_collection(&mut self, name: &str) {
        if !self.collections.contains(&name.to_string()) {
            self.collections.push(name.to_string());
        }
    }

    fn remove_collection(&mut self, name: &str) {
        self.collections.retain(|c| c != name);
    }
}

/// A database instance.
///
/// Databases contain multiple collections, each with its own dimension.
/// This allows multiple embedding models in one database and provides
/// a migration path when switching models.
pub struct Database {
    path: PathBuf,
    manifest: Mutex<DatabaseManifest>,
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Database")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl Database {
    /// Open or create a database at the given path.
    pub fn open(path: impl AsRef<Path>) -> Result<Arc<Self>> {
        let path = path.as_ref().to_path_buf();

        fs::create_dir_all(&path)
            .map_err(Error::io_err(&path, "failed to create database directory"))?;

        let manifest = DatabaseManifest::load(&path)?.unwrap_or_else(DatabaseManifest::new);

        Ok(Arc::new(Self {
            path,
            manifest: Mutex::new(manifest),
        }))
    }

    /// Get the database path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Create a new collection.
    ///
    /// Returns the collection handle. The collection directory is created
    /// and the collection is registered in the database manifest.
    pub fn create_collection(
        self: &Arc<Self>,
        name: &str,
        config: CollectionConfig,
    ) -> Result<Collection> {
        let collection_path = self.path.join(name);

        // Check if collection already exists
        if collection_path.exists() {
            return Err(Error::CollectionExists {
                name: name.to_string(),
            });
        }

        // Create collection directory
        fs::create_dir_all(&collection_path)
            .map_err(Error::io_err(&collection_path, "failed to create collection directory"))?;

        fs::create_dir_all(collection_path.join("segments"))
            .map_err(Error::io_err(&collection_path, "failed to create segments directory"))?;

        // Acquire lock
        let lock = CollectionLock::acquire(&collection_path, name)?;

        // Create manifest
        let manifest = manifest::Manifest::new(config.clone());
        manifest.save(collection_path.join(manifest::MANIFEST_FILE_NAME))?;

        // Create empty WAL
        let wal_path = collection_path.join(wal::WAL_FILE_NAME);
        Wal::create(&wal_path)?;

        // Update database manifest
        {
            let mut db_manifest = self.manifest.lock().unwrap();
            db_manifest.add_collection(name);
            db_manifest.save(&self.path)?;
        }

        // Open and return collection (passing the pre-acquired lock)
        Collection::open_with_lock(self.clone(), name, config, lock)
    }

    /// Get an existing collection.
    ///
    /// Opens the collection with the stored configuration.
    pub fn get_collection(self: &Arc<Self>, name: &str) -> Result<Collection> {
        let collection_path = self.path.join(name);

        if !collection_path.exists() {
            return Err(Error::CollectionNotFound {
                name: name.to_string(),
            });
        }

        // Load collection manifest to get config
        let manifest_path = collection_path.join(manifest::MANIFEST_FILE_NAME);
        let manifest = manifest::Manifest::load(&manifest_path)?.ok_or_else(|| {
            Error::Corruption {
                file: manifest_path.clone(),
                offset: 0,
                message: "collection manifest not found".to_string(),
            }
        })?;

        Collection::open(self.clone(), name, manifest.config)
    }

    /// List all collection names.
    pub fn list_collections(&self) -> Vec<String> {
        let manifest = self.manifest.lock().unwrap();
        manifest.collections.clone()
    }
}

/// A collection of vectors with the same dimension.
///
/// Uses LSM-Lite architecture:
/// - Memtable for recent writes (HashMap + SoA)
/// - Immutable segments on disk (memory-mapped)
/// - WAL for crash recovery
/// - HNSW index for approximate search (optional, loaded on demand)
pub struct Collection {
    /// Database reference
    _db: Arc<Database>,
    /// Collection name
    name: String,
    /// Collection path
    path: PathBuf,
    /// Configuration (immutable)
    config: CollectionConfig,
    /// Exclusive lock (released on drop)
    _lock: CollectionLock,
    /// Manifest manager
    manifest: Mutex<manifest::ManifestManager>,
    /// Active memtable (protected by RwLock)
    memtable: RwLock<Memtable>,
    /// Immutable segments (atomic updates via ArcSwap)
    segments: ArcSwap<Vec<Arc<Segment>>>,
    /// WAL for durability
    wal: Mutex<Wal>,
    /// Next internal ID counter
    next_internal_id: AtomicU64,
    /// HNSW index for approximate search (loaded on demand)
    hnsw_index: ArcSwap<Option<Arc<HnswIndex>>>,
}

impl std::fmt::Debug for Collection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Collection")
            .field("name", &self.name)
            .field("path", &self.path)
            .field("config", &self.config)
            .field("segments", &self.segments.load().len())
            .finish_non_exhaustive()
    }
}

impl Collection {
    /// Open an existing collection.
    ///
    /// This is called from Database::get_collection.
    fn open(db: Arc<Database>, name: &str, config: CollectionConfig) -> Result<Self> {
        let path = db.path().join(name);

        // Acquire exclusive lock
        let lock = CollectionLock::acquire(&path, name)?;

        Self::open_with_lock(db, name, config, lock)
    }

    /// Open a collection with a pre-acquired lock.
    ///
    /// This is called from Database::create_collection which already holds the lock.
    fn open_with_lock(
        db: Arc<Database>,
        name: &str,
        config: CollectionConfig,
        lock: CollectionLock,
    ) -> Result<Self> {
        let path = db.path().join(name);

        // Clean up orphan temp files from interrupted compactions
        let _ = compaction::cleanup_temp_files(&path);

        // Load manifest
        let manifest_path = path.join(manifest::MANIFEST_FILE_NAME);
        let manifest_manager =
            manifest::ManifestManager::open(&manifest_path, Some(config.clone()))?;

        // Load existing segments
        let mut segments = Vec::new();
        for entry in &manifest_manager.manifest().segments {
            let segment_path = path.join("segments").join(&entry.filename);
            let segment = Segment::open(&segment_path)?;
            segments.push(segment);
        }

        // Open WAL
        let wal_path = path.join(wal::WAL_FILE_NAME);
        let wal = Wal::open(&wal_path, Some(manifest_manager.last_wal_seq()))?;

        // Create empty memtable
        let memtable = Memtable::new(config.dim);

        // Load HNSW index if it exists
        let hnsw_index = Self::load_hnsw_index(&path, &manifest_manager);

        // Create collection
        let collection = Self {
            _db: db,
            name: name.to_string(),
            path: path.clone(),
            config: config.clone(),
            _lock: lock,
            manifest: Mutex::new(manifest_manager),
            memtable: RwLock::new(memtable),
            segments: ArcSwap::new(Arc::new(segments)),
            wal: Mutex::new(wal),
            next_internal_id: AtomicU64::new(0),
            hnsw_index: ArcSwap::new(Arc::new(hnsw_index)),
        };

        // Recover from WAL
        collection.recover()?;

        Ok(collection)
    }

    /// Recover from WAL replay.
    fn recover(&self) -> Result<()> {
        let last_wal_seq = self.manifest.lock().unwrap().last_wal_seq();
        let mut wal = self.wal.lock().unwrap();
        let mut memtable = self.memtable.write().unwrap();

        wal.replay(last_wal_seq + 1, self.config.dim, |_seq, record| {
            match record {
                wal::Record::Insert(body) => {
                    let doc = Document {
                        id: body.id,
                        vector: body.vector,
                        payload: body.payload.and_then(|p| serde_json::from_str(&p).ok()),
                    };
                    memtable.insert(doc)?;
                }
                wal::Record::Delete(body) => {
                    memtable.delete(&body.id);
                }
            }
            Ok(())
        })?;

        // Update manifest with new last_wal_seq
        let new_last_seq = wal.next_seq() - 1;
        drop(wal);
        drop(memtable);

        if new_last_seq > last_wal_seq {
            let mut manifest = self.manifest.lock().unwrap();
            manifest.manifest_mut().set_last_wal_seq(new_last_seq);
            manifest.save()?;
        }

        Ok(())
    }

    /// Get the collection name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the collection configuration.
    pub fn config(&self) -> &CollectionConfig {
        &self.config
    }

    /// Get the collection path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Insert or replace a document.
    ///
    /// If a document with the same ID already exists, it is replaced.
    /// The operation is appended to the WAL for durability.
    pub fn insert(&self, doc: Document) -> Result<()> {
        // Validate dimension
        if doc.vector.len() != self.config.dim {
            return Err(Error::WrongDimension {
                expected: self.config.dim,
                got: doc.vector.len(),
            });
        }

        // Create WAL record
        let record = wal::insert_record(&doc);

        // Append to WAL
        let mut wal = self.wal.lock().unwrap();
        let seq = match self.config.durability {
            Durability::Buffered => wal.append(&record, self.config.dim)?,
            Durability::FdatasyncEachBatch => wal.append_and_sync(&record, self.config.dim)?,
        };
        drop(wal);

        // Insert into memtable
        let mut memtable = self.memtable.write().unwrap();
        memtable.insert(doc)?;
        drop(memtable);

        // Check if flush is needed (flush updates manifest)
        self.check_flush()?;

        Ok(())
    }

    /// Insert multiple documents in a single batch.
    ///
    /// More efficient than N individual inserts as it uses a single WAL entry
    /// and single sync (if durability requires).
    ///
    /// Batch size is limited to 64MB to prevent unbounded WAL records.
    pub fn insert_batch(&self, docs: Vec<Document>) -> Result<()> {
        if docs.is_empty() {
            return Ok(());
        }

        // Validate all documents first
        for doc in &docs {
            if doc.vector.len() != self.config.dim {
                return Err(Error::WrongDimension {
                    expected: self.config.dim,
                    got: doc.vector.len(),
                });
            }
        }

        // Check batch size limit (simplified - real implementation would check serialized size)
        let estimated_size: usize = docs.iter().map(|d| d.vector.len() * 4 + d.id.len()).sum();
        if estimated_size > wal::MAX_BATCH_SIZE {
            return Err(Error::invalid_arg(
                "batch",
                format!(
                    "batch size {} exceeds maximum {}",
                    estimated_size,
                    wal::MAX_BATCH_SIZE
                ),
            ));
        }

        // Append each document as separate WAL record (batch is just single transaction)
        let mut wal = self.wal.lock().unwrap();
        let mut last_seq = 0u64;

        for doc in &docs {
            let record = wal::insert_record(doc);
            last_seq = wal.append(&record, self.config.dim)?;
        }

        // Sync once after all appends if required
        if self.config.durability == Durability::FdatasyncEachBatch {
            wal.sync()?;
        }

        drop(wal);

        // Insert all into memtable
        let mut memtable = self.memtable.write().unwrap();
        for doc in docs {
            memtable.insert(doc)?;
        }
        drop(memtable);

        // Check flush (flush updates manifest)

        self.check_flush()?;

        Ok(())
    }

    /// Get a document by ID.
    ///
    /// Searches the memtable first, then all segments (newest to oldest).
    pub fn get(&self, id: &str) -> Result<Option<Document>> {
        // Check memtable first
        let memtable = self.memtable.read().unwrap();
        if let Some((doc, vector)) = memtable.get_by_external(id) {
            return Ok(Some(Document {
                id: doc.external_id.clone(),
                vector: vector.to_vec(),
                payload: doc.payload.clone(),
            }));
        }
        drop(memtable);

        // Search segments (newest first)
        let segments = self.segments.load();
        for segment in segments.iter().rev() {
            if let Some(internal_id) = segment.get_internal_id(id) {
                let vector = segment.get_vector(internal_id).unwrap_or(&[]).to_vec();
                let payload = segment.get_payload(internal_id);
                return Ok(Some(Document {
                    id: id.to_string(),
                    vector,
                    payload,
                }));
            }
        }

        Ok(None)
    }

    /// Delete a document by ID (soft delete).
    ///
    /// The document is marked as deleted in the memtable and a delete
    /// record is appended to the WAL. The document is physically removed
    /// during compaction.
    pub fn delete(&self, id: &str) -> Result<bool> {
        // Create delete record
        let record = wal::delete_record(id);

        // Append to WAL
        let mut wal = self.wal.lock().unwrap();
        let seq = match self.config.durability {
            Durability::Buffered => wal.append(&record, 0)?, // dim not needed for delete
            Durability::FdatasyncEachBatch => wal.append_and_sync(&record, 0)?,
        };
        drop(wal);

        // Mark as deleted in memtable
        let mut memtable = self.memtable.write().unwrap();
        let existed = memtable.delete(id).is_some();
        drop(memtable);

        Ok(existed)
    }

    /// Flush the memtable to a new segment.
    ///
    /// 1. Freeze current memtable
    /// 2. Create new empty memtable for writes
    /// 3. Write frozen memtable to new segment
    /// 4. Update manifest with new segment
    /// 5. Reset WAL
    ///
    /// This is a synchronous operation that blocks writes.
    pub fn flush(&self) -> Result<()> {
        // Get exclusive lock on memtable and swap with empty
        let mut memtable_guard = self.memtable.write().unwrap();
        let memtable = std::mem::replace(&mut *memtable_guard, Memtable::new(self.config.dim));
        drop(memtable_guard);

        // Skip if empty
        if memtable.is_empty() {
            return Ok(());
        }

        // Freeze memtable
        let frozen = memtable.freeze();
        let doc_count = frozen.active_count();

        if doc_count == 0 {
            return Ok(());
        }

        // Generate segment filename
        let segment_id = self.segments.load().len() + 1;
        let segment_filename = format!("{:04}.ndb", segment_id);
        let segment_path = self.path.join("segments").join(&segment_filename);

        // Build segment from frozen memtable
        let mut builder = SegmentBuilder::new(self.config.dim);
        for (_internal_id, external_id, vector, payload) in frozen.iter_active_with_payload() {
            let doc = Document {
                id: external_id.to_string(),
                vector: vector.to_vec(),
                payload: payload.cloned(),
            };
            builder.add(doc)?;
        }

        // Write segment file
        builder.build(&segment_path)?;

        // Load new segment
        let new_segment = Segment::open(&segment_path)?;

        // Update segments list (atomic)
        let mut new_segments = (*self.segments.load().clone()).clone();
        new_segments.push(new_segment);
        self.segments.store(Arc::new(new_segments));

        // Update manifest with new segment and reset last_wal_seq (WAL is reset)
        let id_range_start = frozen.id_mapping().next_id() - doc_count as u32;
        let id_range_end = frozen.id_mapping().next_id();

        let mut manifest = self.manifest.lock().unwrap();
        manifest.manifest_mut().add_segment(SegmentEntry {
            filename: segment_filename,
            doc_count: doc_count as u64,
            id_range: (id_range_start, id_range_end),
        });
        manifest.manifest_mut().set_last_wal_seq(0); // WAL reset
        manifest.save()?;
        drop(manifest);

        // Reset WAL
        let mut wal = self.wal.lock().unwrap();
        wal.reset()?;

        Ok(())
    }

    /// Explicitly sync the WAL to disk.
    ///
    /// This ensures all pending writes are durable.
    pub fn sync(&self) -> Result<()> {
        let mut wal = self.wal.lock().unwrap();
        wal.sync()
    }

    /// Check if flush is needed based on WAL size
    fn check_flush(&self) -> Result<()> {
        let wal = self.wal.lock().unwrap();
        let size = wal.file_size()?;

        if size >= WAL_FLUSH_THRESHOLD {
            drop(wal);
            self.flush()?;
        }

        Ok(())
    }

    /// Compact the collection to reclaim space from deleted documents.
    ///
    /// This operation:
    /// 1. Merges all segments into a single new segment
    /// 2. Removes deleted documents physically
    /// 3. Rebuilds the HNSW index (if one exists)
    /// 4. Updates the manifest atomically
    ///
    /// Compaction is a synchronous operation that blocks until complete.
    /// It is crash-safe: if interrupted, old segments remain valid via
    /// the old manifest.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Insert some documents
    /// coll.insert(doc1)?;
    /// coll.insert(doc2)?;
    /// coll.flush()?;
    ///
    /// // Delete some documents (soft delete)
    /// coll.delete("doc1")?;
    ///
    /// // Compact to reclaim space
    /// let result = coll.compact()?;
    /// println!("Reduced from {} to {} documents", result.docs_before, result.docs_after);
    /// ```
    pub fn compact(&self) -> Result<CompactionResult> {
        // Collect deleted IDs BEFORE flushing - flush replaces the memtable
        let deleted_ids = {
            let memtable = self.memtable.read().unwrap();
            compaction::collect_deleted_ids(&memtable)
        };

        // Flush the memtable to ensure all data is in segments
        self.flush()?;

        // Get current segments
        let segments = self.segments.load();
        
        // If no segments, nothing to compact
        if segments.is_empty() {
            return Ok(CompactionResult {
                docs_before: 0,
                docs_after: 0,
                segments_merged: 0,
                new_segment: std::path::PathBuf::new(),
                index_rebuilt: false,
            });
        }

        // Check if we have an index to rebuild
        let should_rebuild_index = self.has_index();

        // Perform compaction
        let mut manifest_guard = self.manifest.lock().unwrap();
        let result = compaction::compact_with_deleted_ids(
            &segments,
            &deleted_ids,
            self.config.dim,
            &self.path,
            manifest_guard.manifest_mut(),
            should_rebuild_index,
        )?;
        drop(manifest_guard);

        // Update in-memory state
        drop(segments);

        if result.docs_after > 0 {
            // Load new segment
            let new_segment = Segment::open(&result.new_segment)?;
            self.segments.store(Arc::new(vec![new_segment]));

            // Reload HNSW index if it was rebuilt
            if result.index_rebuilt {
                let index_path = self.path.join("index.hnsw");
                if index_path.exists() {
                    let bytes = std::fs::read(&index_path)
                        .map_err(Error::io_err(&index_path, "failed to read HNSW index"))?;
                    if let Ok(index) = HnswIndex::from_bytes(&bytes) {
                        self.hnsw_index.store(Arc::new(Some(Arc::new(index))));
                    }
                }
            }
        } else {
            // All documents deleted - clear segments and index
            self.segments.store(Arc::new(vec![]));
            self.hnsw_index.store(Arc::new(None));

            // Delete index file if it exists
            let index_path = self.path.join("index.hnsw");
            if index_path.exists() {
                let _ = std::fs::remove_file(&index_path);
            }
        }

        // Reset WAL
        let mut wal = self.wal.lock().unwrap();
        wal.reset()?;

        Ok(result)
    }

    /// Get collection statistics
    pub fn stats(&self) -> CollectionStats {
        let memtable = self.memtable.read().unwrap();
        let segments = self.segments.load();

        CollectionStats {
            memtable_docs: memtable.active_count(),
            segment_count: segments.len(),
            total_segment_docs: segments.iter().map(|s| s.doc_count()).sum(),
        }
    }

    /// Search for similar vectors.
    ///
    /// Performs similarity search using either exact (brute-force) or
    /// approximate (HNSW) search depending on the search configuration.
    ///
    /// # Example
    ///
    /// ```ignore
    /// // Exact search (default)
    /// let results = collection.search(
    ///     Search::new(&query_vector)
    ///         .top_k(10)
    ///         .distance(Distance::Cosine)
    /// )?;
    ///
    /// // Approximate search using HNSW
    /// let results = collection.search(
    ///     Search::new(&query_vector)
    ///         .top_k(10)
    ///         .distance(Distance::Cosine)
    ///         .approximate(true)
    ///         .ef(100)
    /// )?;
    /// ```
    ///
    /// # Performance
    ///
    /// - Exact search: O(N) scan over all vectors
    /// - Approximate search: O(log N) using HNSW (requires index)
    ///
    /// If approximate search is requested but no index exists, falls back
    /// to exact search.
    pub fn search(&self, search: &Search<'_>) -> Result<Vec<Match>> {
        // Use HNSW if requested and available
        if search.is_approximate() {
            if let Some(result) = self.search_hnsw(search)? {
                return Ok(result);
            }
            // Fallback to exact search if HNSW not available
        }

        // Exact search
        let memtable = self.memtable.read().unwrap();
        let segments = self.segments.load();
        search::exact_search(&memtable, &segments, search)
    }

    /// Search using HNSW index.
    ///
    /// Returns None if no index is available.
    /// Applies post-filtering if a filter is provided in the search.
    fn search_hnsw(&self, search: &Search<'_>) -> Result<Option<Vec<Match>>> {

        let hnsw = self.hnsw_index.load();
        let index = match hnsw.as_ref() {
            Some(idx) => idx,
            None => return Ok(None),
        };

        // Check distance metric compatibility
        if index.distance() != search.distance_metric() {
            // Index was built with different metric, can't use it
            return Ok(None);
        }

        let query = search.vector();
        let top_k = search.top_k_value();
        let ef = search.ef_value().unwrap_or_else(|| index.params().ef_search);
        let filter = search.filter_ref();

        // Get segments for vector lookup
        let segments = self.segments.load();

        // If filtering is needed, fetch more candidates to account for filtered-out docs
        // This is a simple heuristic: request 2x top_k, then filter
        let fetch_k = if filter.is_some() { top_k * 2 } else { top_k };

        // Search the index
        let hnsw_results = index.search(query, fetch_k, ef, |id| {
            // Look up vector by internal ID across segments
            for segment in segments.iter() {
                if let Some(vector) = segment.get_vector(id) {
                    return Some(vector.to_vec());
                }
            }
            None
        })?;

        // Convert to Match results with filtering
        let mut matches = Vec::with_capacity(hnsw_results.len());
        for (internal_id, distance) in hnsw_results {
            // Find the document in segments
            for segment in segments.iter() {
                if let Some(external_id) = segment.get_external_id(internal_id) {
                    let payload = segment.get_payload(internal_id);
                    
                    // Apply filter
                    if let Some(filter) = filter {
                        if let Some(ref p) = payload {
                            if !filter.evaluate(p) {
                                break; // Filter doesn't match, skip this document
                            }
                        } else {
                            // No payload but filter exists - document is excluded
                            break;
                        }
                    }
                    
                    // Convert distance to score based on metric
                    let score = match index.distance() {
                        Distance::DotProduct => -distance, // Negate back
                        Distance::Cosine => 1.0 - distance, // Convert distance back to similarity
                        Distance::Euclidean => -distance, // Negate for "higher is better"
                    };
                    
                    matches.push(Match {
                        id: external_id,
                        score,
                        payload,
                    });
                    break;
                }
            }
        }

        // Sort by score descending
        matches.sort_by(|a, b| {
            b.score.partial_cmp(&a.score).unwrap_or(Ordering::Equal)
        });

        // Truncate to top_k
        matches.truncate(top_k);

        Ok(Some(matches))
    }

    /// Load HNSW index from disk if it exists.
    fn load_hnsw_index(
        path: &Path,
        manifest_manager: &manifest::ManifestManager,
    ) -> Option<Arc<HnswIndex>> {
        let manifest = manifest_manager.manifest();
        let index_file = manifest.index_file()?;
        let index_path = path.join(index_file);

        if !index_path.exists() {
            return None;
        }

        let bytes = std::fs::read(&index_path).ok()?;
        HnswIndex::from_bytes(&bytes).ok().map(Arc::new)
    }

    /// Build and save the HNSW index.
    ///
    /// This operation can be expensive for large collections.
    /// It scans all segments and memtable, then builds the index from scratch.
    pub fn rebuild_index(&self) -> Result<()> {
        let params = HnswParams::default();
        let distance = Distance::Cosine; // TODO: make configurable

        // Collect all vectors from segments
        let segments = self.segments.load();
        let mut vectors: Vec<(u32, Vec<f32>)> = Vec::new();

        for segment in segments.iter() {
            for internal_id in 0..segment.doc_count() as u32 {
                if let Some(vector) = segment.get_vector(internal_id) {
                    vectors.push((internal_id, vector.to_vec()));
                }
            }
        }

        // Also collect vectors from memtable
        let memtable = self.memtable.read().unwrap();
        for (internal_id, _doc, vector) in memtable.iter() {
            vectors.push((internal_id, vector.to_vec()));
        }
        drop(memtable);

        if vectors.is_empty() {
            return Err(Error::invalid_arg("vectors", "cannot build index from empty collection"));
        }

        // Build the index
        let dim = self.config.dim;
        let mut builder = hnsw::HnswBuilder::new(dim, distance, params);

        for (_, vector) in vectors {
            builder.add(vector)?;
        }

        let index = builder.build()?;

        // Save to disk
        let index_filename = format!("index.hnsw");
        let index_path = self.path.join(&index_filename);
        let bytes = index.to_bytes()?;
        std::fs::write(&index_path, bytes)
            .map_err(Error::io_err(&index_path, "failed to write HNSW index"))?;

        // Update manifest
        let mut manifest = self.manifest.lock().unwrap();
        manifest.manifest_mut().set_index_file(Some(index_filename));
        manifest.manifest_mut().increment_index_generation();
        manifest.save()?;

        // Update in-memory index
        self.hnsw_index.store(Arc::new(Some(Arc::new(index))));

        Ok(())
    }

    /// Delete the HNSW index.
    ///
    /// Removes the index file and clears the in-memory index.
    /// Subsequent approximate searches will fall back to exact search.
    pub fn delete_index(&self) -> Result<()> {
        // Get current index file
        let index_file = {
            let manifest = self.manifest.lock().unwrap();
            manifest.manifest().index_file().map(|s| s.to_string())
        };

        // Remove file if exists
        if let Some(filename) = index_file {
            let index_path = self.path.join(&filename);
            if index_path.exists() {
                std::fs::remove_file(&index_path)
                    .map_err(Error::io_err(&index_path, "failed to delete HNSW index"))?;
            }
        }

        // Update manifest
        let mut manifest = self.manifest.lock().unwrap();
        manifest.manifest_mut().set_index_file(None);
        manifest.save()?;

        // Clear in-memory index
        self.hnsw_index.store(Arc::new(None));

        Ok(())
    }

    /// Check if an HNSW index exists.
    pub fn has_index(&self) -> bool {
        self.hnsw_index.load().is_some()
    }
}

/// Collection statistics
#[derive(Debug, Clone)]
pub struct CollectionStats {
    /// Number of documents in memtable
    pub memtable_docs: usize,
    /// Number of segments
    pub segment_count: usize,
    /// Total documents across all segments
    pub total_segment_docs: usize,
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_doc(id: &str, dim: usize) -> Document {
        Document {
            id: id.to_string(),
            vector: (0..dim).map(|i| i as f32).collect(),
            payload: Some(serde_json::json!({"id": id})),
        }
    }

    #[test]
    fn test_database_open() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();
        assert_eq!(db.path(), temp_dir.path());
    }

    #[test]
    fn test_collection_config() {
        let config = CollectionConfig::new(768).with_durability(Durability::FdatasyncEachBatch);

        assert_eq!(config.dim, 768);
        assert_eq!(config.durability, Durability::FdatasyncEachBatch);
    }

    #[test]
    fn test_create_and_get_collection() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();

        let config = CollectionConfig::new(4);
        {
            let _coll = db.create_collection("test_create_get", config.clone()).unwrap();
            // Collection is locked while held
        }

        // After dropping, should be able to get it back
        let coll2 = db.get_collection("test_create_get").unwrap();
        assert_eq!(coll2.name(), "test_create_get");
        assert_eq!(coll2.config().dim, 4);

        // List should include it
        let collections = db.list_collections();
        assert!(collections.contains(&"test_create_get".to_string()));
    }

    #[test]
    fn test_duplicate_collection_fails() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();

        let config = CollectionConfig::new(4);
        {
            let _coll = db.create_collection("test_dup", config.clone()).unwrap();
        }

        let err = db.create_collection("test_dup", config).unwrap_err();
        assert!(matches!(err, Error::CollectionExists { name } if name == "test_dup"));
    }

    #[test]
    fn test_collection_not_found() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();

        let err = db.get_collection("nonexistent").unwrap_err();
        assert!(matches!(err, Error::CollectionNotFound { name } if name == "nonexistent"));
    }

    #[test]
    fn test_collection_insert_and_get() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();

        let coll = db
            .create_collection("test_insert_get", CollectionConfig::new(4))
            .unwrap();

        let doc = create_test_doc("doc1", 4);
        coll.insert(doc.clone()).unwrap();

        let retrieved = coll.get("doc1").unwrap().unwrap();
        assert_eq!(retrieved.id, "doc1");
        assert_eq!(retrieved.vector, doc.vector);
    }

    #[test]
    fn test_collection_dimension_mismatch() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();

        let coll = db
            .create_collection("test_dim", CollectionConfig::new(4))
            .unwrap();

        let doc = Document {
            id: "doc1".to_string(),
            vector: vec![1.0, 2.0, 3.0], // Wrong dimension
            payload: None,
        };

        let err = coll.insert(doc).unwrap_err();
        assert!(matches!(err, Error::WrongDimension { expected: 4, got: 3 }));
    }

    #[test]
    fn test_collection_insert_batch() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();

        let coll = db
            .create_collection("test_batch", CollectionConfig::new(4))
            .unwrap();

        let docs: Vec<_> = (0..10).map(|i| create_test_doc(&format!("doc{}", i), 4)).collect();
        coll.insert_batch(docs).unwrap();

        // Verify all inserted
        for i in 0..10 {
            let retrieved = coll.get(&format!("doc{}", i)).unwrap().unwrap();
            assert_eq!(retrieved.id, format!("doc{}", i));
        }
    }

    #[test]
    fn test_collection_delete() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();

        let coll = db
            .create_collection("test_delete", CollectionConfig::new(4))
            .unwrap();

        coll.insert(create_test_doc("doc1", 4)).unwrap();
        assert!(coll.get("doc1").unwrap().is_some());

        let deleted = coll.delete("doc1").unwrap();
        assert!(deleted);
        assert!(coll.get("doc1").unwrap().is_none());

        // Deleting non-existent returns false
        let deleted = coll.delete("nonexistent").unwrap();
        assert!(!deleted);
    }

    #[test]
    fn test_collection_flush() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();

        let coll = db
            .create_collection("test_flush", CollectionConfig::new(4))
            .unwrap();

        // Insert some docs
        for i in 0..5 {
            coll.insert(create_test_doc(&format!("doc{}", i), 4))
                .unwrap();
        }

        // Flush
        coll.flush().unwrap();

        // Should still be accessible
        for i in 0..5 {
            let retrieved = coll.get(&format!("doc{}", i)).unwrap().unwrap();
            assert_eq!(retrieved.id, format!("doc{}", i));
        }

        // Stats should show segment
        let stats = coll.stats();
        assert_eq!(stats.memtable_docs, 0); // Flushed
        assert_eq!(stats.segment_count, 1);
        assert_eq!(stats.total_segment_docs, 5);
    }

    #[test]
    fn test_collection_persistence() {
        let temp_dir = TempDir::new().unwrap();

        // Create and populate collection
        {
            let db = Database::open(temp_dir.path()).unwrap();
            let coll = db
                .create_collection("test_persist", CollectionConfig::new(4))
                .unwrap();

            coll.insert(create_test_doc("doc1", 4)).unwrap();
            coll.insert(create_test_doc("doc2", 4)).unwrap();
            coll.sync().unwrap(); // Ensure durability
        }

        // Reopen and verify
        {
            let db = Database::open(temp_dir.path()).unwrap();
            let coll = db.get_collection("test_persist").unwrap();

            let doc1 = coll.get("doc1").unwrap().unwrap();
            assert_eq!(doc1.id, "doc1");

            let doc2 = coll.get("doc2").unwrap().unwrap();
            assert_eq!(doc2.id, "doc2");
        }
    }
}
