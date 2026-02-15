//! nDB Node.js Native Bindings
//!
//! This crate provides N-API bindings for the nDB vector database,
//! enabling native-speed vector operations from Node.js.

use napi::bindgen_prelude::*;
use napi_derive::napi;
use std::sync::Arc;

use ndb::{Database as RustDatabase, Collection as RustCollection, CollectionConfig, Document};
use ndb::{Durability, Distance, Search, Filter};

// FilterBuilder is exposed via napi

/// Database class for Node.js
///
/// Wraps the Rust Database type and provides JS-friendly methods.
#[napi]
pub struct Database {
    inner: Arc<RustDatabase>,
}

#[napi]
impl Database {
    /// Open or create a database at the given path
    #[napi(constructor)]
    pub fn new(path: String) -> Result<Self> {
        let inner = RustDatabase::open(&path)
            .map_err(|e| Error::from_reason(format!("Failed to open database: {}", e)))?;
        Ok(Self { inner })
    }

    /// Create a new collection
    #[napi]
    pub fn create_collection(
        &self,
        name: String,
        dimension: u32,
        options: Option<CollectionOptions>,
    ) -> Result<Collection> {
        let durability = options
            .and_then(|o| o.durability)
            .map(|d| match d.as_str() {
                "sync" => Durability::FdatasyncEachBatch,
                _ => Durability::Buffered,
            })
            .unwrap_or_default();

        let config = CollectionConfig {
            dim: dimension as usize,
            durability,
        };

        let coll = self
            .inner
            .create_collection(&name, config)
            .map_err(|e| Error::from_reason(format!("Failed to create collection: {}", e)))?;

        Ok(Collection {
            inner: coll,
            _db: self.inner.clone(),
        })
    }

    /// Get an existing collection
    #[napi]
    pub fn get_collection(&self, name: String) -> Result<Collection> {
        let coll = self
            .inner
            .get_collection(&name)
            .map_err(|e| Error::from_reason(format!("Failed to get collection: {}", e)))?;

        Ok(Collection {
            inner: coll,
            _db: self.inner.clone(),
        })
    }

    /// List all collection names
    #[napi]
    pub fn list_collections(&self) -> Vec<String> {
        self.inner.list_collections()
    }
}

/// Collection options for creation
#[napi(object)]
pub struct CollectionOptions {
    pub durability: Option<String>,
}

/// Collection class for Node.js
///
/// Wraps the Rust Collection type and provides JS-friendly methods.
#[napi]
pub struct Collection {
    inner: RustCollection,
    // Keep reference to database to prevent premature drop
    _db: Arc<RustDatabase>,
}

#[napi]
impl Collection {
    /// Get the collection name
    #[napi(getter)]
    pub fn name(&self) -> String {
        self.inner.name().to_string()
    }

    /// Get the collection configuration
    #[napi(getter)]
    pub fn config(&self) -> CollectionConfigJs {
        let config = self.inner.config();
        CollectionConfigJs {
            dim: config.dim as u32,
            durability: match config.durability {
                Durability::FdatasyncEachBatch => "sync".to_string(),
                Durability::Buffered => "buffered".to_string(),
            },
        }
    }

    /// Insert a single document
    #[napi]
    pub fn insert(&self, id: String, vector: Vec<f64>, payload: Option<String>) -> Result<()> {
        let vector_f32: Vec<f32> = vector.iter().map(|v| *v as f32).collect();
        
        let payload_json = payload
            .map(|p| serde_json::from_str(&p))
            .transpose()
            .map_err(|e| Error::from_reason(format!("Invalid JSON payload: {}", e)))?;

        let doc = Document {
            id,
            vector: vector_f32,
            payload: payload_json,
        };

        self.inner
            .insert(doc)
            .map_err(|e| Error::from_reason(format!("Failed to insert document: {}", e)))?;

        Ok(())
    }

    /// Insert multiple documents in a batch
    #[napi]
    pub fn insert_batch(&self, docs: Vec<InsertDoc>) -> Result<()> {
        let documents: Vec<Document> = docs
            .into_iter()
            .map(|d| {
                let payload_json = d
                    .payload
                    .map(|p| serde_json::from_str(&p))
                    .transpose()
                    .map_err(|e| Error::from_reason(format!("Invalid JSON payload: {}", e)))?;

                let vector_f32: Vec<f32> = d.vector.iter().map(|v| *v as f32).collect();

                Ok(Document {
                    id: d.id,
                    vector: vector_f32,
                    payload: payload_json,
                })
            })
            .collect::<Result<Vec<_>>>()?;

        self.inner
            .insert_batch(documents)
            .map_err(|e| Error::from_reason(format!("Failed to insert batch: {}", e)))?;

        Ok(())
    }

    /// Get a document by ID
    #[napi]
    pub fn get(&self, id: String) -> Result<Option<DocumentJs>> {
        let doc = self
            .inner
            .get(&id)
            .map_err(|e| Error::from_reason(format!("Failed to get document: {}", e)))?;

        Ok(doc.map(|d| DocumentJs {
            id: d.id,
            vector: d.vector.iter().map(|v| *v as f64).collect(),
            payload: d.payload.map(|p| p.to_string()),
        }))
    }

    /// Delete a document by ID
    #[napi]
    pub fn delete(&self, id: String) -> Result<bool> {
        self.inner
            .delete(&id)
            .map_err(|e| Error::from_reason(format!("Failed to delete document: {}", e)))
    }

    /// Search for similar vectors
    #[napi]
    pub fn search(&self, options: SearchOptions) -> Result<Vec<MatchJs>> {
        let query_f32: Vec<f32> = options.vector.iter().map(|v| *v as f32).collect();
        let mut search = Search::new(&query_f32);

        if let Some(top_k) = options.top_k {
            search = search.top_k(top_k as usize);
        }

        if let Some(distance) = options.distance {
            let metric = match distance.as_str() {
                "dot" => Distance::DotProduct,
                "euclidean" => Distance::Euclidean,
                _ => Distance::Cosine,
            };
            search = search.distance(metric);
        }

        if let Some(approximate) = options.approximate {
            search = search.approximate(approximate);
        }

        if let Some(ef) = options.ef {
            search = search.ef(ef as usize);
        }

        if let Some(filter_json) = options.filter {
            let filter: Filter = serde_json::from_str(&filter_json)
                .map_err(|e| Error::from_reason(format!("Invalid filter JSON: {}", e)))?;
            search = search.filter(filter);
        }

        let results = self
            .inner
            .search(&search)
            .map_err(|e| Error::from_reason(format!("Search failed: {}", e)))?;

        Ok(results
            .into_iter()
            .map(|m| MatchJs {
                id: m.id,
                score: m.score as f64,
                payload: m.payload.map(|p| p.to_string()),
            })
            .collect())
    }

    /// Flush memtable to disk
    #[napi]
    pub fn flush(&self) -> Result<()> {
        self.inner
            .flush()
            .map_err(|e| Error::from_reason(format!("Flush failed: {}", e)))
    }

    /// Sync WAL to disk
    #[napi]
    pub fn sync(&self) -> Result<()> {
        self.inner
            .sync()
            .map_err(|e| Error::from_reason(format!("Sync failed: {}", e)))
    }

    /// Compact the collection
    #[napi]
    pub fn compact(&self) -> Result<CompactionResultJs> {
        let result = self
            .inner
            .compact()
            .map_err(|e| Error::from_reason(format!("Compaction failed: {}", e)))?;

        Ok(CompactionResultJs {
            docs_before: result.docs_before as u32,
            docs_after: result.docs_after as u32,
            segments_merged: result.segments_merged as u32,
            index_rebuilt: result.index_rebuilt,
        })
    }

    /// Rebuild the HNSW index
    #[napi]
    pub fn rebuild_index(&self) -> Result<()> {
        self.inner
            .rebuild_index()
            .map_err(|e| Error::from_reason(format!("Failed to rebuild index: {}", e)))
    }

    /// Delete the HNSW index
    #[napi]
    pub fn delete_index(&self) -> Result<()> {
        self.inner
            .delete_index()
            .map_err(|e| Error::from_reason(format!("Failed to delete index: {}", e)))
    }

    /// Check if HNSW index exists
    #[napi]
    pub fn has_index(&self) -> bool {
        self.inner.has_index()
    }

    /// Get collection statistics
    #[napi(getter)]
    pub fn stats(&self) -> CollectionStatsJs {
        let stats = self.inner.stats();
        CollectionStatsJs {
            memtable_docs: stats.memtable_docs as u32,
            segment_count: stats.segment_count as u32,
            total_segment_docs: stats.total_segment_docs as u32,
        }
    }
}

/// Collection configuration (JS representation)
#[napi(object)]
pub struct CollectionConfigJs {
    pub dim: u32,
    pub durability: String,
}

/// Document for insertion
#[napi(object)]
pub struct InsertDoc {
    pub id: String,
    pub vector: Vec<f64>,
    pub payload: Option<String>,
}

/// Document (JS representation)
#[napi(object)]
pub struct DocumentJs {
    pub id: String,
    pub vector: Vec<f64>,
    pub payload: Option<String>,
}

/// Search options
#[napi(object)]
pub struct SearchOptions {
    pub vector: Vec<f64>,
    pub top_k: Option<u32>,
    pub distance: Option<String>,
    pub approximate: Option<bool>,
    pub ef: Option<u32>,
    pub filter: Option<String>,
}

/// Search match result (JS representation)
#[napi(object)]
pub struct MatchJs {
    pub id: String,
    pub score: f64,
    pub payload: Option<String>,
}

/// Compaction result (JS representation)
#[napi(object)]
pub struct CompactionResultJs {
    pub docs_before: u32,
    pub docs_after: u32,
    pub segments_merged: u32,
    pub index_rebuilt: bool,
}

/// Collection statistics (JS representation)
#[napi(object)]
pub struct CollectionStatsJs {
    pub memtable_docs: u32,
    pub segment_count: u32,
    pub total_segment_docs: u32,
}

/// FilterBuilder for constructing query filters
/// 
/// Produces JSON in the format expected by the Rust Filter enum:
/// {"Eq": {"field": "name", "value": "value"}}
#[napi]
pub struct FilterBuilder;

#[napi]
impl FilterBuilder {
    /// Create an equality filter: field == value
    #[napi]
    pub fn eq(field: String, value: serde_json::Value) -> String {
        let filter = serde_json::json!({
            "Eq": { "field": field, "value": value }
        });
        filter.to_string()
    }

    /// Create a greater-than filter: field > value
    #[napi]
    pub fn gt(field: String, value: serde_json::Value) -> String {
        let filter = serde_json::json!({
            "Gt": { "field": field, "value": value }
        });
        filter.to_string()
    }

    /// Create a greater-than-or-equal filter: field >= value
    #[napi]
    pub fn gte(field: String, value: serde_json::Value) -> String {
        let filter = serde_json::json!({
            "Gte": { "field": field, "value": value }
        });
        filter.to_string()
    }

    /// Create a less-than filter: field < value
    #[napi]
    pub fn lt(field: String, value: serde_json::Value) -> String {
        let filter = serde_json::json!({
            "Lt": { "field": field, "value": value }
        });
        filter.to_string()
    }

    /// Create a less-than-or-equal filter: field <= value
    #[napi]
    pub fn lte(field: String, value: serde_json::Value) -> String {
        let filter = serde_json::json!({
            "Lte": { "field": field, "value": value }
        });
        filter.to_string()
    }

    /// Create a not-equal filter: field != value
    #[napi]
    pub fn ne(field: String, value: serde_json::Value) -> String {
        let filter = serde_json::json!({
            "Ne": { "field": field, "value": value }
        });
        filter.to_string()
    }

    /// Create an "in" filter: field IN values
    #[napi]
    pub fn in_(field: String, values: Vec<serde_json::Value>) -> String {
        let filter = serde_json::json!({
            "In": { "field": field, "values": values }
        });
        filter.to_string()
    }

    /// Create a logical AND filter: all filters must match
    #[napi]
    pub fn and(filters: Vec<String>) -> Result<String> {
        let mut filter_objs = Vec::with_capacity(filters.len());
        for f in filters {
            let obj: Filter = serde_json::from_str(&f)
                .map_err(|e| Error::from_reason(format!("Invalid filter JSON: {}", e)))?;
            filter_objs.push(obj);
        }

        let filter = Filter::And(filter_objs);
        Ok(serde_json::to_string(&filter).unwrap())
    }

    /// Create a logical OR filter: any filter must match
    #[napi]
    pub fn or(filters: Vec<String>) -> Result<String> {
        let mut filter_objs = Vec::with_capacity(filters.len());
        for f in filters {
            let obj: Filter = serde_json::from_str(&f)
                .map_err(|e| Error::from_reason(format!("Invalid filter JSON: {}", e)))?;
            filter_objs.push(obj);
        }

        let filter = Filter::Or(filter_objs);
        Ok(serde_json::to_string(&filter).unwrap())
    }
}
