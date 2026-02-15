//! In-memory memtable for recent writes.
//!
//! The memtable provides:
//! - HashMap<u32, Document> for O(1) lookups by internal ID
//! - SoA (Structure of Arrays) layout for SIMD-friendly scans
//! - Delete bitmap for soft deletes (reconstructed from WAL)

use crate::error::{Error, Result};
use crate::id::IdMapping;
use crate::segment::Document;
use std::collections::{HashMap, HashSet};

/// In-memory memtable for recent writes.
///
/// Uses a hybrid storage approach:
/// - HashMap for O(1) document lookups by internal ID
/// - SoA vector buffer for SIMD-friendly scanning
///
/// When frozen for flush, the memtable is converted to a segment.
#[derive(Debug)]
pub struct Memtable {
    /// Vector dimension (fixed per collection)
    dimension: usize,
    /// ID mapping (external string -> internal u32)
    id_mapping: IdMapping,
    /// Document storage by internal ID
    documents: HashMap<u32, MemtableDoc>,
    /// SoA vector buffer: all vectors packed contiguously
    /// Layout: [vec0[0..dim], vec1[0..dim], ..., vecN[0..dim]]
    vector_buffer: Vec<f32>,
    /// Next position in vector_buffer for new vectors
    next_buffer_pos: usize,
    /// Deleted document set (external_id -> deleted)
    /// Uses external IDs since internal IDs are local to each memtable/segment
    deleted: HashSet<String>,
    /// Total size estimate in bytes (for flush threshold)
    estimated_size: usize,
}

/// A document in the memtable (without vector - stored in SoA buffer)
#[derive(Debug, Clone)]
pub struct MemtableDoc {
    /// Internal ID
    pub internal_id: u32,
    /// External ID
    pub external_id: String,
    /// Start index in vector_buffer
    pub vector_offset: usize,
    /// Optional payload
    pub payload: Option<serde_json::Value>,
}

/// Iterator over memtable documents in SoA order
pub struct MemtableIter<'a> {
    memtable: &'a Memtable,
    current: usize,
}

impl<'a> Iterator for MemtableIter<'a> {
    type Item = (u32, &'a str, &'a [f32]);

    fn next(&mut self) -> Option<Self::Item> {
        loop {
            if self.current >= self.memtable.next_buffer_pos {
                return None;
            }

            // Find the internal ID for this buffer position
            let offset = self.current;
            let internal_id = (offset / self.memtable.dimension) as u32;

            // Skip deleted documents
            if self.memtable.is_deleted(internal_id) {
                self.current += self.memtable.dimension;
                continue;
            }

            // Get document and vector
            if let Some(doc) = self.memtable.documents.get(&internal_id) {
                let vector = &self.memtable.vector_buffer[offset..offset + self.memtable.dimension];
                self.current += self.memtable.dimension;
                return Some((internal_id, &doc.external_id, vector));
            }

            self.current += self.memtable.dimension;
        }
    }
}

impl Memtable {
    /// Create a new empty memtable with the given dimension
    pub fn new(dimension: usize) -> Self {
        Self {
            dimension,
            id_mapping: IdMapping::new(),
            documents: HashMap::new(),
            vector_buffer: Vec::new(),
            next_buffer_pos: 0,
            deleted: HashSet::new(),
            estimated_size: 0,
        }
    }

    /// Create with pre-allocated capacity
    pub fn with_capacity(dimension: usize, doc_capacity: usize) -> Self {
        Self {
            dimension,
            id_mapping: IdMapping::with_capacity(doc_capacity),
            documents: HashMap::with_capacity(doc_capacity),
            vector_buffer: Vec::with_capacity(doc_capacity * dimension),
            next_buffer_pos: 0,
            deleted: HashSet::new(),
            estimated_size: 0,
        }
    }

    /// Get vector dimension
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Number of documents (including deleted)
    pub fn len(&self) -> usize {
        self.documents.len()
    }

    /// Check if empty
    pub fn is_empty(&self) -> bool {
        self.documents.is_empty()
    }

    /// Number of active (non-deleted) documents
    pub fn active_count(&self) -> usize {
        self.documents
            .keys()
            .filter(|&&id| !self.is_deleted(id))
            .count()
    }

    /// Check if a document is deleted by external ID
    pub fn is_deleted_by_external(&self, external_id: &str) -> bool {
        self.deleted.contains(external_id)
    }

    /// Check if a document is deleted by internal ID
    pub fn is_deleted(&self, internal_id: u32) -> bool {
        if let Some(external_id) = self.id_mapping.get_external(internal_id) {
            self.deleted.contains(external_id)
        } else {
            false
        }
    }

    /// Mark a document as deleted by external ID
    pub fn mark_deleted_by_external(&mut self, external_id: &str) {
        self.deleted.insert(external_id.to_string());
    }

    /// Mark a document as deleted by internal ID
    pub fn mark_deleted(&mut self, internal_id: u32) {
        if let Some(external_id) = self.id_mapping.get_external(internal_id) {
            self.deleted.insert(external_id.to_string());
        }
    }

    /// Collect all deleted document IDs.
    pub fn collect_deleted_ids(&self) -> HashSet<String> {
        self.deleted.clone()
    }

    /// Get the ID mapping
    pub fn id_mapping(&self) -> &IdMapping {
        &self.id_mapping
    }

    /// Get the internal ID for an external ID
    pub fn get_internal_id(&self, external_id: &str) -> Option<u32> {
        self.id_mapping.get_internal(external_id)
    }

    /// Get a document by internal ID
    pub fn get(&self, internal_id: u32) -> Option<(&MemtableDoc, &[f32])> {
        if self.is_deleted(internal_id) {
            return None;
        }

        let doc = self.documents.get(&internal_id)?;
        let vector = &self.vector_buffer[doc.vector_offset..doc.vector_offset + self.dimension];
        Some((doc, vector))
    }

    /// Get a document by external ID
    pub fn get_by_external(&self, external_id: &str) -> Option<(&MemtableDoc, &[f32])> {
        let internal_id = self.id_mapping.get_internal(external_id)?;
        self.get(internal_id)
    }

    /// Insert or replace a document
    ///
    /// If the document already exists, it is replaced (old vector remains in buffer).
    /// Returns the internal ID assigned to the document.
    pub fn insert(&mut self, doc: Document) -> Result<u32> {
        if doc.vector.len() != self.dimension {
            return Err(Error::WrongDimension {
                expected: self.dimension,
                got: doc.vector.len(),
            });
        }

        // Get or assign internal ID
        let internal_id = self.id_mapping.insert(doc.id.clone());

        // Store vector in SoA buffer
        let vector_offset = self.next_buffer_pos;
        self.vector_buffer.extend_from_slice(&doc.vector);
        self.next_buffer_pos += self.dimension;

        // Store document metadata
        let memtable_doc = MemtableDoc {
            internal_id,
            external_id: doc.id,
            vector_offset,
            payload: doc.payload,
        };

        // Update size estimate
        self.estimated_size += self.dimension * 4 + memtable_doc.external_id.len();

        // Remove from deleted if it was previously deleted (re-inserting undoes delete)
        let external_id = memtable_doc.external_id.clone();
        self.deleted.remove(&external_id);

        // Insert (may replace existing)
        self.documents.insert(internal_id, memtable_doc);

        Ok(internal_id)
    }

    /// Delete a document by external ID (soft delete)
    ///
    /// Returns the internal ID if the document existed in memtable, None otherwise.
    /// Note: The delete is tracked by external ID, so it works for documents
    /// in segments too.
    pub fn delete(&mut self, external_id: &str) -> Option<u32> {
        // Always track the delete by external ID
        self.mark_deleted_by_external(external_id);
        
        // Return internal ID if document exists in memtable
        let internal_id = self.id_mapping.get_internal(external_id)?;
        if self.documents.contains_key(&internal_id) {
            Some(internal_id)
        } else {
            None
        }
    }

    /// Iterate over all active (non-deleted) documents
    pub fn iter(&self) -> impl Iterator<Item = (u32, &str, &[f32])> {
        MemtableIter {
            memtable: self,
            current: 0,
        }
    }

    /// Get the vector buffer (SoA layout) for SIMD scans
    ///
    /// Returns slice of all vectors, including deleted ones.
    /// Use `iter()` to skip deleted documents.
    pub fn vector_buffer(&self) -> &[f32] {
        &self.vector_buffer[..self.next_buffer_pos]
    }

    /// Get raw access to documents HashMap
    pub fn documents(&self) -> &HashMap<u32, MemtableDoc> {
        &self.documents
    }

    /// Estimated size in bytes
    pub fn estimated_size(&self) -> usize {
        self.estimated_size
    }

    /// Freeze this memtable (convert to immutable for flush)
    ///
    /// Returns a FrozenMemtable that can be used to build a segment.
    pub fn freeze(self) -> FrozenMemtable {
        FrozenMemtable {
            dimension: self.dimension,
            id_mapping: self.id_mapping,
            documents: self.documents,
            vector_buffer: self.vector_buffer,
            deleted: self.deleted,
        }
    }

    /// Create a memtable from a frozen one (for testing)
    #[cfg(test)]
    pub fn from_frozen(frozen: FrozenMemtable) -> Self {
        let next_buffer_pos = frozen.vector_buffer.len();
        Self {
            dimension: frozen.dimension,
            id_mapping: frozen.id_mapping,
            documents: frozen.documents,
            vector_buffer: frozen.vector_buffer,
            next_buffer_pos,
            deleted: frozen.deleted,
            estimated_size: 0,
        }
    }
}

/// A frozen memtable ready for flush to segment.
///
/// Immutable snapshot of the memtable at flush time.
#[derive(Debug)]
pub struct FrozenMemtable {
    dimension: usize,
    id_mapping: IdMapping,
    documents: HashMap<u32, MemtableDoc>,
    vector_buffer: Vec<f32>,
    deleted: HashSet<String>,
}

impl FrozenMemtable {
    /// Get vector dimension
    pub fn dimension(&self) -> usize {
        self.dimension
    }

    /// Get the ID mapping
    pub fn id_mapping(&self) -> &IdMapping {
        &self.id_mapping
    }

    /// Get all active documents as iterator
    pub fn iter_active(&self) -> impl Iterator<Item = (u32, &str, &[f32])> {
        let dim = self.dimension;
        self.documents
            .values()
            .filter(move |doc| !self.is_deleted(doc.internal_id))
            .map(move |doc| {
                let vector = &self.vector_buffer[doc.vector_offset..doc.vector_offset + dim];
                (doc.internal_id, doc.external_id.as_str(), vector)
            })
    }

    /// Get all active documents with payloads as iterator
    pub fn iter_active_with_payload(&self) -> impl Iterator<Item = (u32, &str, &[f32], Option<&serde_json::Value>)> {
        let dim = self.dimension;
        self.documents
            .values()
            .filter(move |doc| !self.is_deleted(doc.internal_id))
            .map(move |doc| {
                let vector = &self.vector_buffer[doc.vector_offset..doc.vector_offset + dim];
                (doc.internal_id, doc.external_id.as_str(), vector, doc.payload.as_ref())
            })
    }

    /// Check if a document is deleted by internal ID
    fn is_deleted(&self, internal_id: u32) -> bool {
        if let Some(external_id) = self.id_mapping.get_external(internal_id) {
            self.deleted.contains(external_id)
        } else {
            false
        }
    }

    /// Number of active documents
    pub fn active_count(&self) -> usize {
        self.documents
            .values()
            .filter(|doc| !self.is_deleted(doc.internal_id))
            .count()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn create_doc(id: &str, dim: usize) -> Document {
        Document {
            id: id.to_string(),
            vector: (0..dim).map(|i| i as f32).collect(),
            payload: Some(serde_json::json!({"id": id})),
        }
    }

    #[test]
    fn test_memtable_insert_and_get() {
        let mut memtable = Memtable::new(4);

        let doc = create_doc("doc1", 4);
        let internal_id = memtable.insert(doc.clone()).unwrap();

        // Get by internal ID
        let (retrieved, vector) = memtable.get(internal_id).unwrap();
        assert_eq!(retrieved.external_id, "doc1");
        assert_eq!(vector, &[0.0, 1.0, 2.0, 3.0]);

        // Get by external ID
        let (retrieved2, vector2) = memtable.get_by_external("doc1").unwrap();
        assert_eq!(retrieved2.internal_id, internal_id);
        assert_eq!(vector2, vector);
    }

    #[test]
    fn test_memtable_dimension_mismatch() {
        let mut memtable = Memtable::new(4);

        let doc = Document {
            id: "doc1".to_string(),
            vector: vec![1.0, 2.0, 3.0], // Wrong dimension
            payload: None,
        };

        let err = memtable.insert(doc).unwrap_err();
        assert!(matches!(err, Error::WrongDimension { expected: 4, got: 3 }));
    }

    #[test]
    fn test_memtable_delete() {
        let mut memtable = Memtable::new(4);

        let doc = create_doc("doc1", 4);
        memtable.insert(doc).unwrap();

        assert!(memtable.get_by_external("doc1").is_some());

        // Soft delete
        let deleted_id = memtable.delete("doc1");
        assert!(deleted_id.is_some());

        // Should not be found
        assert!(memtable.get_by_external("doc1").is_none());

        // But internal ID mapping should still exist
        assert!(memtable.get_internal_id("doc1").is_some());
    }

    #[test]
    fn test_memtable_iter() {
        let mut memtable = Memtable::new(4);

        memtable.insert(create_doc("doc1", 4)).unwrap();
        memtable.insert(create_doc("doc2", 4)).unwrap();
        memtable.insert(create_doc("doc3", 4)).unwrap();

        // Delete middle doc
        memtable.delete("doc2");

        // Iterator should skip deleted
        let docs: Vec<_> = memtable.iter().collect();
        assert_eq!(docs.len(), 2);
        assert_eq!(docs[0].1, "doc1");
        assert_eq!(docs[1].1, "doc3");
    }

    #[test]
    fn test_memtable_replace() {
        let mut memtable = Memtable::new(4);

        // Insert doc1
        let doc1 = create_doc("doc1", 4);
        let id1 = memtable.insert(doc1).unwrap();

        // Replace with new vector
        let doc2 = Document {
            id: "doc1".to_string(),
            vector: vec![10.0, 20.0, 30.0, 40.0],
            payload: Some(serde_json::json!({"updated": true})),
        };
        let id2 = memtable.insert(doc2).unwrap();

        // Same internal ID
        assert_eq!(id1, id2);

        // New vector value
        let (_, vector) = memtable.get(id2).unwrap();
        assert_eq!(vector, &[10.0, 20.0, 30.0, 40.0]);
    }

    #[test]
    fn test_memtable_soa_layout() {
        let mut memtable = Memtable::new(4);

        memtable.insert(create_doc("doc1", 4)).unwrap();
        memtable.insert(create_doc("doc2", 4)).unwrap();

        // SoA buffer should have vectors packed contiguously
        let buffer = memtable.vector_buffer();
        assert_eq!(buffer.len(), 8); // 2 docs * 4 dims

        // First vector at positions 0-3
        assert_eq!(&buffer[0..4], &[0.0, 1.0, 2.0, 3.0]);
        // Second vector at positions 4-7
        assert_eq!(&buffer[4..8], &[0.0, 1.0, 2.0, 3.0]);
    }

    #[test]
    fn test_frozen_memtable() {
        let mut memtable = Memtable::new(4);

        memtable.insert(create_doc("doc1", 4)).unwrap();
        memtable.insert(create_doc("doc2", 4)).unwrap();
        memtable.delete("doc1");

        let frozen = memtable.freeze();

        // Only active docs
        let docs: Vec<_> = frozen.iter_active().collect();
        assert_eq!(docs.len(), 1);
        assert_eq!(docs[0].1, "doc2");
    }

    #[test]
    fn test_active_count() {
        let mut memtable = Memtable::new(4);

        memtable.insert(create_doc("doc1", 4)).unwrap();
        memtable.insert(create_doc("doc2", 4)).unwrap();
        memtable.insert(create_doc("doc3", 4)).unwrap();

        assert_eq!(memtable.active_count(), 3);

        memtable.delete("doc2");

        assert_eq!(memtable.active_count(), 2);
        assert_eq!(memtable.len(), 3); // Total includes deleted
    }
}
