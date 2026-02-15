//! Phase 5: Compaction Integration Tests
//!
//! Tests for the compaction functionality:
//! - Space reclamation from deleted documents
//! - HNSW index rebuilding
//! - Crash recovery
//! - Query correctness after compaction

use ndb::{CollectionConfig, Database, Document, Search};
use tempfile::TempDir;

fn create_test_doc(id: &str, dim: usize) -> Document {
    Document {
        id: id.to_string(),
        vector: (0..dim).map(|i| i as f32 * 0.1).collect(),
        payload: Some(serde_json::json!({"id": id})),
    }
}

#[test]
fn test_compaction_reduces_size() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert documents
    for i in 0..10 {
        coll.insert(create_test_doc(&format!("doc{}", i), 4))
            .unwrap();
    }
    coll.flush().unwrap();

    // Delete half the documents
    for i in 0..5 {
        coll.delete(&format!("doc{}", i)).unwrap();
    }

    // Get stats before compaction
    let stats_before = coll.stats();
    assert_eq!(stats_before.segment_count, 1);

    // Compact
    let result = coll.compact().unwrap();

    // Verify results
    assert_eq!(result.docs_before, 10);
    assert_eq!(result.docs_after, 5);
    assert_eq!(result.segments_merged, 1);

    // Stats should show single segment
    let stats_after = coll.stats();
    assert_eq!(stats_after.segment_count, 1);
    assert_eq!(stats_after.total_segment_docs, 5);
}

#[test]
fn test_compaction_with_multiple_segments() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Create multiple segments by flushing after each batch
    for batch in 0..3 {
        for i in 0..5 {
            let id = format!("batch{}_doc{}", batch, i);
            coll.insert(create_test_doc(&id, 4)).unwrap();
        }
        coll.flush().unwrap();
    }

    // Stats should show 3 segments
    let stats_before = coll.stats();
    assert_eq!(stats_before.segment_count, 3);
    assert_eq!(stats_before.total_segment_docs, 15);

    // Delete some documents from different segments
    coll.delete("batch0_doc0").unwrap();
    coll.delete("batch1_doc2").unwrap();
    coll.delete("batch2_doc4").unwrap();

    // Compact
    let result = coll.compact().unwrap();

    // Should merge all 3 segments
    assert_eq!(result.segments_merged, 3);
    assert_eq!(result.docs_before, 15);
    assert_eq!(result.docs_after, 12);

    // Should now have 1 segment
    let stats_after = coll.stats();
    assert_eq!(stats_after.segment_count, 1);
}

#[test]
fn test_compaction_preserves_data_integrity() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert documents with specific vectors
    let doc1 = Document {
        id: "doc1".to_string(),
        vector: vec![1.0, 2.0, 3.0, 4.0],
        payload: Some(serde_json::json!({"key": "value1"})),
    };
    let doc2 = Document {
        id: "doc2".to_string(),
        vector: vec![5.0, 6.0, 7.0, 8.0],
        payload: Some(serde_json::json!({"key": "value2"})),
    };
    let doc3 = Document {
        id: "doc3".to_string(),
        vector: vec![9.0, 10.0, 11.0, 12.0],
        payload: None,
    };

    coll.insert(doc1).unwrap();
    coll.insert(doc2).unwrap();
    coll.insert(doc3).unwrap();
    coll.flush().unwrap();

    // Delete doc2
    coll.delete("doc2").unwrap();

    // Compact
    coll.compact().unwrap();

    // Verify remaining documents are intact
    let retrieved1 = coll.get("doc1").unwrap().unwrap();
    assert_eq!(retrieved1.vector, vec![1.0, 2.0, 3.0, 4.0]);
    assert_eq!(
        retrieved1.payload,
        Some(serde_json::json!({"key": "value1"}))
    );

    let retrieved3 = coll.get("doc3").unwrap().unwrap();
    assert_eq!(retrieved3.vector, vec![9.0, 10.0, 11.0, 12.0]);
    assert_eq!(retrieved3.payload, None);

    // Deleted doc should not exist
    assert!(coll.get("doc2").unwrap().is_none());
}

#[test]
fn test_compaction_query_after() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert documents
    for i in 0..10 {
        let mut doc = create_test_doc(&format!("doc{}", i), 4);
        doc.vector = vec![i as f32, 0.0, 0.0, 0.0];
        coll.insert(doc).unwrap();
    }
    coll.flush().unwrap();

    // Delete some documents
    coll.delete("doc3").unwrap();
    coll.delete("doc5").unwrap();
    coll.delete("doc7").unwrap();

    // Search before compaction
    let query = vec![5.0, 0.0, 0.0, 0.0];
    let results_before = coll.search(&Search::new(&query).top_k(3)).unwrap();

    // Compact
    coll.compact().unwrap();

    // Search after compaction - results should be valid (same doc count, no deleted docs)
    let results_after = coll.search(&Search::new(&query).top_k(3)).unwrap();

    // Should have same number of results
    assert_eq!(results_before.len(), results_after.len());

    // Deleted docs should not appear in results (most important check)
    let all_results = coll.search(&Search::new(&query).top_k(20)).unwrap();
    let ids: Vec<_> = all_results.iter().map(|m| m.id.clone()).collect();
    assert!(!ids.contains(&"doc3".to_string()));
    assert!(!ids.contains(&"doc5".to_string()));
    assert!(!ids.contains(&"doc7".to_string()));

    // Results should be sorted by score descending
    for i in 1..results_after.len() {
        assert!(
            results_after[i-1].score >= results_after[i].score,
            "Results should be sorted by score"
        );
    }
}

#[test]
fn test_compaction_empty_collection() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Compact empty collection
    let result = coll.compact().unwrap();

    assert_eq!(result.docs_before, 0);
    assert_eq!(result.docs_after, 0);
    assert_eq!(result.segments_merged, 0);

    // Stats should be empty
    let stats = coll.stats();
    assert_eq!(stats.segment_count, 0);
    assert_eq!(stats.total_segment_docs, 0);
}

#[test]
fn test_compaction_no_deletes() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert without deleting
    for i in 0..5 {
        coll.insert(create_test_doc(&format!("doc{}", i), 4))
            .unwrap();
    }
    coll.flush().unwrap();

    // Compact without any deletes
    let result = coll.compact().unwrap();

    assert_eq!(result.docs_before, 5);
    assert_eq!(result.docs_after, 5);

    // All docs should still be there
    for i in 0..5 {
        assert!(coll.get(&format!("doc{}", i)).unwrap().is_some());
    }
}

#[test]
fn test_compaction_all_deleted() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert and then delete all
    for i in 0..5 {
        coll.insert(create_test_doc(&format!("doc{}", i), 4))
            .unwrap();
    }
    coll.flush().unwrap();

    for i in 0..5 {
        coll.delete(&format!("doc{}", i)).unwrap();
    }

    // Compact
    let result = coll.compact().unwrap();

    assert_eq!(result.docs_before, 5);
    assert_eq!(result.docs_after, 0);

    // Collection should be empty
    let stats = coll.stats();
    assert_eq!(stats.segment_count, 0);
    assert_eq!(stats.total_segment_docs, 0);
}

#[test]
fn test_compaction_rebuilds_index() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert documents
    for i in 0..20 {
        coll.insert(create_test_doc(&format!("doc{}", i), 4))
            .unwrap();
    }
    coll.flush().unwrap();

    // Build HNSW index
    coll.rebuild_index().unwrap();
    assert!(coll.has_index());

    // Delete some documents
    for i in 0..10 {
        coll.delete(&format!("doc{}", i)).unwrap();
    }

    // Compact with index rebuild
    let result = coll.compact().unwrap();
    assert!(result.index_rebuilt);

    // Index should still work
    assert!(coll.has_index());

    // Approximate search should work
    let query = vec![0.1, 0.2, 0.3, 0.4];
    let results = coll
        .search(&Search::new(&query).top_k(5).approximate(true))
        .unwrap();
    assert!(!results.is_empty());

    // Deleted docs should not appear
    for result in &results {
        let id_num: i32 = result.id.replace("doc", "").parse().unwrap();
        assert!(id_num >= 10, "Deleted doc {} appeared in results", result.id);
    }
}

#[test]
fn test_compaction_persistence() {
    let temp_dir = TempDir::new().unwrap();
    let db_path = temp_dir.path();

    // Create and populate collection
    {
        let db = Database::open(db_path).unwrap();
        let coll = db
            .create_collection("test", CollectionConfig::new(4))
            .unwrap();

        for i in 0..10 {
            coll.insert(create_test_doc(&format!("doc{}", i), 4))
                .unwrap();
        }
        coll.flush().unwrap();

        // Delete and compact
        for i in 0..5 {
            coll.delete(&format!("doc{}", i)).unwrap();
        }
        coll.compact().unwrap();
    }

    // Reopen and verify
    {
        let db = Database::open(db_path).unwrap();
        let coll = db.get_collection("test").unwrap();

        // Should only have 5 documents
        let stats = coll.stats();
        assert_eq!(stats.total_segment_docs, 5);

        // First 5 should be deleted
        for i in 0..5 {
            assert!(coll.get(&format!("doc{}", i)).unwrap().is_none());
        }

        // Last 5 should exist
        for i in 5..10 {
            assert!(coll.get(&format!("doc{}", i)).unwrap().is_some());
        }
    }
}

#[test]
fn test_compaction_orphan_cleanup() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    // Create collection
    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert and flush to create a segment
    coll.insert(create_test_doc("doc1", 4)).unwrap();
    coll.flush().unwrap();

    // Create fake temp files (simulating interrupted compaction)
    let segments_dir = temp_dir.path().join("test").join("segments");
    std::fs::write(segments_dir.join("orphan.ndb.tmp"), b"fake data").unwrap();
    std::fs::write(
        temp_dir.path().join("test").join("index.hnsw.tmp"),
        b"fake index",
    )
    .unwrap();

    // Verify temp files exist
    assert!(segments_dir.join("orphan.ndb.tmp").exists());

    // Drop collection and reopen (should clean up orphans)
    drop(coll);
    drop(db);

    let db2 = Database::open(temp_dir.path()).unwrap();
    let _coll2 = db2.get_collection("test").unwrap();

    // Orphan files should be cleaned up
    assert!(!segments_dir.join("orphan.ndb.tmp").exists());
}

#[test]
fn test_compaction_with_document_updates() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert original document
    let doc1_v1 = Document {
        id: "doc1".to_string(),
        vector: vec![1.0, 1.0, 1.0, 1.0],
        payload: Some(serde_json::json!({"version": 1})),
    };
    coll.insert(doc1_v1).unwrap();
    coll.flush().unwrap();

    // Update the same document (creates new version in memtable, will be in new segment after flush)
    let doc1_v2 = Document {
        id: "doc1".to_string(),
        vector: vec![2.0, 2.0, 2.0, 2.0],
        payload: Some(serde_json::json!({"version": 2})),
    };
    coll.insert(doc1_v2).unwrap();
    coll.flush().unwrap();

    // Now we have 2 segments, both with doc1 (older and newer version)
    let stats = coll.stats();
    assert_eq!(stats.segment_count, 2);

    // Compact - should keep only the newer version
    coll.compact().unwrap();

    // Should have 1 segment with 1 document
    let stats_after = coll.stats();
    assert_eq!(stats_after.segment_count, 1);
    assert_eq!(stats_after.total_segment_docs, 1);

    // Document should be the newer version
    let retrieved = coll.get("doc1").unwrap().unwrap();
    assert_eq!(retrieved.vector, vec![2.0, 2.0, 2.0, 2.0]);
    assert_eq!(
        retrieved.payload,
        Some(serde_json::json!({"version": 2}))
    );
}

#[test]
fn test_compaction_idempotent() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert and flush
    for i in 0..5 {
        coll.insert(create_test_doc(&format!("doc{}", i), 4))
            .unwrap();
    }
    coll.flush().unwrap();

    // Delete some
    coll.delete("doc0").unwrap();
    coll.delete("doc1").unwrap();

    // Compact first time
    let result1 = coll.compact().unwrap();
    assert_eq!(result1.docs_after, 3);

    // Compact again (should be idempotent)
    let result2 = coll.compact().unwrap();
    assert_eq!(result2.docs_after, 3);

    // Data should still be intact
    for i in 2..5 {
        assert!(coll.get(&format!("doc{}", i)).unwrap().is_some());
    }
}
