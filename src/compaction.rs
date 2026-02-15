//! Compaction logic for reclaiming space from deleted documents.
//!
//! Compaction merges all segments, removes deleted documents,
//! and rebuilds the HNSW index. This is a synchronous operation
//! that blocks until complete.
//!
//! ## Crash Safety
//!
//! Compaction follows an atomic update protocol:
//! 1. Write new segment to `*.tmp`
//! 2. Write new index to `*.tmp`
//! 3. Update manifest atomically (write-temp + rename)
//! 4. Delete old segment files
//!
//! If compaction crashes at any point:
//! - Temp files are ignored on startup
//! - Old manifest still references valid old segments
//! - Compaction can be safely retried

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::distance::Distance;
use crate::error::{Error, Result};
use crate::hnsw::{HnswBuilder, HnswIndex, HnswParams};
use crate::manifest::{Manifest, SegmentEntry};
use crate::memtable::Memtable;
use crate::segment::{Document, Segment, SegmentBuilder};

/// Result of a compaction operation.
#[derive(Debug, Clone)]
pub struct CompactionResult {
    /// Number of documents before compaction
    pub docs_before: usize,
    /// Number of documents after compaction (deleted removed)
    pub docs_after: usize,
    /// Number of old segments merged
    pub segments_merged: usize,
    /// New segment file path
    pub new_segment: PathBuf,
    /// Whether HNSW index was rebuilt
    pub index_rebuilt: bool,
}

/// Collect all deleted document IDs from the memtable.
pub fn collect_deleted_ids(memtable: &Memtable) -> HashSet<String> {
    memtable.collect_deleted_ids()
}

/// Merge multiple segments, filtering out deleted documents.
///
/// Documents are merged in order, with later segments taking precedence
/// for any ID collisions (supporting updates).
pub fn merge_segments(
    segments: &[Arc<Segment>],
    deleted_ids: &HashSet<String>,
    _dim: usize,
) -> Result<Vec<Document>> {
    let mut merged: Vec<Document> = Vec::new();
    let mut seen_ids: HashSet<String> = HashSet::new();
    
    // Process segments from newest to oldest
    // (Segments are stored in creation order, newest last)
    // This ensures newer versions of documents take precedence
    for segment in segments.iter().rev() {
        for (internal_id, external_id, vector) in segment.iter() {
            // Skip if already seen (newer version exists in later segment)
            if seen_ids.contains(&external_id) {
                continue;
            }
            
            // Mark as seen (whether deleted or not, we don't want older versions)
            seen_ids.insert(external_id.clone());
            
            // Skip if deleted
            if deleted_ids.contains(&external_id) {
                continue;
            }
            
            // Get payload
            let payload = segment.get_payload(internal_id);
            
            merged.push(Document {
                id: external_id,
                vector: vector.to_vec(),
                payload,
            });
        }
    }
    
    Ok(merged)
}

/// Write documents to a new segment file (temporary).
pub fn write_segment_temp(
    docs: &[Document],
    dim: usize,
    segments_dir: &Path,
) -> Result<(PathBuf, Arc<Segment>)> {
    if docs.is_empty() {
        return Err(Error::invalid_arg("docs", "cannot create empty segment"));
    }
    
    // Generate temp filename
    let temp_filename = format!("{:04}.ndb.tmp", 1);
    let temp_path = segments_dir.join(&temp_filename);
    
    // Build segment
    let mut builder = SegmentBuilder::new(dim);
    for doc in docs {
        builder.add(doc.clone())?;
    }
    
    builder.build(&temp_path)?;
    
    // Open the segment
    let segment = Segment::open(&temp_path)?;
    
    Ok((temp_path, segment))
}

/// Build HNSW index from documents.
pub fn build_hnsw_index(docs: &[Document], dim: usize, distance: Distance) -> Result<HnswIndex> {
    if docs.is_empty() {
        return Err(Error::invalid_arg("docs", "cannot build index from empty documents"));
    }
    
    let params = HnswParams::default();
    let mut builder = HnswBuilder::new(dim, distance, params);
    
    for doc in docs.iter() {
        builder.add(doc.vector.clone())?;
    }
    
    builder.build()
}

/// Perform compaction.
///
/// This is the main compaction algorithm:
/// 1. Collect deleted document IDs from memtable
/// 2. Merge all segments, filtering out deleted docs
/// 3. Write new segment to temp file
/// 4. Build and write new HNSW index
/// 5. Update manifest atomically
/// 6. Clean up old files
///
/// Returns statistics about the compaction.
pub fn compact(
    segments: &[Arc<Segment>],
    memtable: &Memtable,
    dim: usize,
    collection_path: &Path,
    manifest: &mut Manifest,
    should_rebuild_index: bool,
) -> Result<CompactionResult> {
    let deleted_ids = collect_deleted_ids(memtable);
    compact_with_deleted_ids(
        segments,
        &deleted_ids,
        dim,
        collection_path,
        manifest,
        should_rebuild_index,
    )
}

/// Perform compaction with pre-collected deleted IDs.
///
/// This variant allows the caller to collect deleted IDs before any
/// operations that might modify the memtable (like flush).
pub fn compact_with_deleted_ids(
    segments: &[Arc<Segment>],
    deleted_ids: &std::collections::HashSet<String>,
    dim: usize,
    collection_path: &Path,
    manifest: &mut Manifest,
    should_rebuild_index: bool,
) -> Result<CompactionResult> {
    let segments_dir = collection_path.join("segments");
    
    // Count documents before
    let docs_before: usize = segments.iter().map(|s| s.doc_count()).sum();
    
    // Merge segments, filtering out deleted docs
    let merged_docs = merge_segments(segments, deleted_ids, dim)?;
    let docs_after = merged_docs.len();
    
    // If no documents remain, clear everything
    if merged_docs.is_empty() {
        // Clear manifest segments
        let old_filenames: Vec<String> = manifest.segment_filenames();
        manifest.remove_segments(&old_filenames);
        manifest.set_index_file(None);
        manifest.set_last_wal_seq(0);
        
        return Ok(CompactionResult {
            docs_before,
            docs_after: 0,
            segments_merged: segments.len(),
            new_segment: PathBuf::new(),
            index_rebuilt: false,
        });
    }
    
    // Write new segment to temp file
    let (temp_segment_path, new_segment) = write_segment_temp(&merged_docs, dim, &segments_dir)?;
    
    // Build new HNSW index if requested
    let index_rebuilt = if should_rebuild_index {
        let index = build_hnsw_index(&merged_docs, dim, Distance::Cosine)?;
        
        // Write index to temp file
        let temp_index_path = collection_path.join("index.hnsw.tmp");
        let bytes = index.to_bytes()?;
        std::fs::write(&temp_index_path, bytes)
            .map_err(Error::io_err(&temp_index_path, "failed to write HNSW index"))?;
        
        // Rename to final index file
        let index_path = collection_path.join("index.hnsw");
        std::fs::rename(&temp_index_path, &index_path)
            .map_err(Error::io_err(&index_path, "failed to rename index file"))?;
        
        manifest.set_index_file(Some("index.hnsw".to_string()));
        manifest.increment_index_generation();
        
        true
    } else {
        false
    };
    
    // Get old segment filenames for cleanup
    let old_filenames: Vec<String> = manifest.segment_filenames();
    
    // Rename temp segment to final name
    let final_segment_filename = format!("{:04}.ndb", 1);
    let final_segment_path = segments_dir.join(&final_segment_filename);
    std::fs::rename(&temp_segment_path, &final_segment_path)
        .map_err(Error::io_err(&final_segment_path, "failed to rename segment file"))?;
    
    // Update manifest atomically
    manifest.remove_segments(&old_filenames);
    manifest.add_segment(SegmentEntry {
        filename: final_segment_filename,
        doc_count: docs_after as u64,
        id_range: (0, docs_after as u32),
    });
    manifest.set_last_wal_seq(0); // WAL will be reset
    
    // Save manifest (atomic rename)
    let manifest_path = collection_path.join("MANIFEST");
    manifest.save(&manifest_path)?;
    
    // Delete old segment files (safe now that manifest is updated)
    // Note: Don't delete the new segment file if it has the same name as an old one
    let new_filename = final_segment_path.file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    for filename in old_filenames {
        if filename == new_filename {
            continue; // Skip the new segment file
        }
        let old_path = segments_dir.join(&filename);
        if old_path.exists() {
            let _ = std::fs::remove_file(&old_path);
        }
    }
    
    Ok(CompactionResult {
        docs_before,
        docs_after,
        segments_merged: segments.len(),
        new_segment: final_segment_path,
        index_rebuilt,
    })
}

/// Clean up orphan temp files on startup.
///
/// Temp files from interrupted compactions are safe to delete.
pub fn cleanup_temp_files(collection_path: &Path) -> Result<usize> {
    let segments_dir = collection_path.join("segments");
    let mut cleaned = 0usize;
    
    // Clean up segment temp files (*.tmp)
    if let Ok(entries) = std::fs::read_dir(&segments_dir) {
        for entry in entries.flatten() {
            let path = entry.path();
            if let Some(ext) = path.extension() {
                if ext == "tmp" {
                    if let Some(name) = path.file_stem() {
                        if name.to_string_lossy().ends_with(".ndb") {
                            let _ = std::fs::remove_file(&path);
                            cleaned += 1;
                        }
                    }
                }
            }
        }
    }
    
    // Clean up index temp files
    let index_tmp = collection_path.join("index.hnsw.tmp");
    if index_tmp.exists() {
        let _ = std::fs::remove_file(&index_tmp);
        cleaned += 1;
    }
    
    // Clean up manifest temp files
    let manifest_tmp = collection_path.join("MANIFEST.tmp");
    if manifest_tmp.exists() {
        let _ = std::fs::remove_file(&manifest_tmp);
        cleaned += 1;
    }
    
    Ok(cleaned)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::id::IdMapping;
    use tempfile::TempDir;

    fn create_test_doc(id: &str, dim: usize) -> Document {
        Document {
            id: id.to_string(),
            vector: (0..dim).map(|i| i as f32).collect(),
            payload: Some(serde_json::json!({"id": id})),
        }
    }

    fn create_test_segment(path: &Path, docs: &[Document]) -> Arc<Segment> {
        if docs.is_empty() {
            panic!("Cannot create empty segment");
        }
        
        let dim = docs[0].vector.len();
        let mut builder = SegmentBuilder::new(dim);
        for doc in docs {
            builder.add(doc.clone()).unwrap();
        }
        builder.build(path).unwrap();
        Segment::open(path).unwrap()
    }

    #[test]
    fn test_merge_segments_basic() {
        let temp_dir = TempDir::new().unwrap();
        let dim = 4;
        
        // Create segment 1 with docs 1, 2
        let path1 = temp_dir.path().join("0001.ndb");
        let seg1 = create_test_segment(
            &path1,
            &[
                create_test_doc("doc1", dim),
                create_test_doc("doc2", dim),
            ],
        );
        
        // Create segment 2 with docs 3, 4
        let path2 = temp_dir.path().join("0002.ndb");
        let seg2 = create_test_segment(
            &path2,
            &[
                create_test_doc("doc3", dim),
                create_test_doc("doc4", dim),
            ],
        );
        
        let segments = vec![seg1, seg2];
        let deleted: HashSet<String> = HashSet::new();
        
        let merged = merge_segments(&segments, &deleted, dim).unwrap();
        
        assert_eq!(merged.len(), 4);
        let ids: Vec<_> = merged.iter().map(|d| d.id.clone()).collect();
        assert!(ids.contains(&"doc1".to_string()));
        assert!(ids.contains(&"doc2".to_string()));
        assert!(ids.contains(&"doc3".to_string()));
        assert!(ids.contains(&"doc4".to_string()));
    }

    #[test]
    fn test_merge_segments_with_deletes() {
        let temp_dir = TempDir::new().unwrap();
        let dim = 4;
        
        // Create segment with docs
        let path = temp_dir.path().join("0001.ndb");
        let seg = create_test_segment(
            &path,
            &[
                create_test_doc("doc1", dim),
                create_test_doc("doc2", dim),
                create_test_doc("doc3", dim),
            ],
        );
        
        let segments = vec![seg];
        let mut deleted: HashSet<String> = HashSet::new();
        deleted.insert("doc2".to_string());
        
        let merged = merge_segments(&segments, &deleted, dim).unwrap();
        
        assert_eq!(merged.len(), 2);
        let ids: Vec<_> = merged.iter().map(|d| d.id.clone()).collect();
        assert!(ids.contains(&"doc1".to_string()));
        assert!(!ids.contains(&"doc2".to_string()));
        assert!(ids.contains(&"doc3".to_string()));
    }

    #[test]
    fn test_merge_segments_newer_wins() {
        let temp_dir = TempDir::new().unwrap();
        let dim = 4;
        
        // Create segment 1 with doc1 (old version)
        let path1 = temp_dir.path().join("0001.ndb");
        let mut old_doc = create_test_doc("doc1", dim);
        old_doc.vector = vec![1.0, 1.0, 1.0, 1.0];
        let seg1 = create_test_segment(&path1, &[old_doc]);
        
        // Create segment 2 with doc1 (new version)
        let path2 = temp_dir.path().join("0002.ndb");
        let mut new_doc = create_test_doc("doc1", dim);
        new_doc.vector = vec![2.0, 2.0, 2.0, 2.0];
        let seg2 = create_test_segment(&path2, &[new_doc]);
        
        let segments = vec![seg1, seg2];
        let deleted: HashSet<String> = HashSet::new();
        
        let merged = merge_segments(&segments, &deleted, dim).unwrap();
        
        // Should only have 1 doc1, and it should be the newer version
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0].id, "doc1");
        assert_eq!(merged[0].vector, vec![2.0, 2.0, 2.0, 2.0]);
    }

    #[test]
    fn test_cleanup_temp_files() {
        let temp_dir = TempDir::new().unwrap();
        let segments_dir = temp_dir.path().join("segments");
        std::fs::create_dir(&segments_dir).unwrap();
        
        // Create some temp files
        std::fs::write(segments_dir.join("0001.ndb.tmp"), b"temp").unwrap();
        std::fs::write(segments_dir.join("0002.ndb.tmp"), b"temp").unwrap();
        std::fs::write(temp_dir.path().join("index.hnsw.tmp"), b"temp").unwrap();
        std::fs::write(temp_dir.path().join("MANIFEST.tmp"), b"temp").unwrap();
        
        // Create a non-temp file that should NOT be deleted
        std::fs::write(segments_dir.join("0001.ndb"), b"real").unwrap();
        
        let cleaned = cleanup_temp_files(temp_dir.path()).unwrap();
        assert_eq!(cleaned, 4);
        
        // Verify temp files are gone
        assert!(!segments_dir.join("0001.ndb.tmp").exists());
        assert!(!segments_dir.join("0002.ndb.tmp").exists());
        assert!(!temp_dir.path().join("index.hnsw.tmp").exists());
        assert!(!temp_dir.path().join("MANIFEST.tmp").exists());
        
        // Verify real file remains
        assert!(segments_dir.join("0001.ndb").exists());
    }

    #[test]
    fn test_compact_empty_collection() {
        let temp_dir = TempDir::new().unwrap();
        let collection_path = temp_dir.path();
        let segments_dir = collection_path.join("segments");
        std::fs::create_dir(&segments_dir).unwrap();
        
        let segments: Vec<Arc<Segment>> = vec![];
        let memtable = Memtable::new(4);
        let mut manifest = Manifest::new(crate::CollectionConfig::new(4));
        
        let result = compact(
            &segments,
            &memtable,
            4,
            collection_path,
            &mut manifest,
            false,
        ).unwrap();
        
        assert_eq!(result.docs_before, 0);
        assert_eq!(result.docs_after, 0);
        assert_eq!(result.segments_merged, 0);
    }
}
