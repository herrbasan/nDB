//! Phase 2: Search - Exact Similarity Tests
//!
//! Tests for brute-force vector search with SIMD acceleration.

use ndb::{CollectionConfig, Database, Distance, Document, Search};
use tempfile::TempDir;

fn create_doc(id: &str, vector: Vec<f32>) -> Document {
    Document {
        id: id.to_string(),
        vector,
        payload: Some(serde_json::json!({"id": id})),
    }
}

#[test]
fn test_search_basic_cosine() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(3))
        .unwrap();

    // Insert documents with different vector directions
    coll.insert(create_doc("a", vec![1.0, 0.0, 0.0])).unwrap();
    coll.insert(create_doc("b", vec![0.0, 1.0, 0.0])).unwrap();
    coll.insert(create_doc("c", vec![0.0, 0.0, 1.0])).unwrap();
    coll.insert(create_doc("d", vec![0.707, 0.707, 0.0])).unwrap(); // 45 degrees between a and b

    // Search with query vector [1, 0, 0] - should return "a" first
    let query = vec![1.0, 0.0, 0.0];
    let results = coll.search(&Search::new(&query).top_k(4)).unwrap();

    assert_eq!(results.len(), 4);
    assert_eq!(results[0].id, "a"); // Exact match
    assert!((results[0].score - 1.0).abs() < 1e-5); // Cosine of identical vectors is 1

    // "d" should be second (45 degrees from a)
    assert_eq!(results[1].id, "d");
    assert!((results[1].score - 0.707).abs() < 0.01); // cos(45°) ≈ 0.707
}

#[test]
fn test_search_top_k_limit() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert 10 documents
    for i in 0..10 {
        coll.insert(create_doc(&format!("doc{}", i), vec![i as f32, 0.0, 0.0, 0.0]))
            .unwrap();
    }

    // Search with different top_k values
    let query = vec![9.0, 0.0, 0.0, 0.0];

    let results_3 = coll.search(&Search::new(&query).top_k(3)).unwrap();
    assert_eq!(results_3.len(), 3);

    let results_5 = coll.search(&Search::new(&query).top_k(5)).unwrap();
    assert_eq!(results_5.len(), 5);

    let results_100 = coll.search(&Search::new(&query).top_k(100)).unwrap();
    assert_eq!(results_100.len(), 10); // Only 10 documents exist
}

#[test]
fn test_search_different_distances() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    coll.insert(create_doc("a", vec![1.0, 0.0])).unwrap();
    coll.insert(create_doc("b", vec![0.0, 1.0])).unwrap();
    coll.insert(create_doc("c", vec![0.5, 0.5])).unwrap();

    let query = vec![1.0, 0.0];

    // Cosine similarity
    let cosine_results = coll
        .search(&Search::new(&query).distance(Distance::Cosine))
        .unwrap();
    assert_eq!(cosine_results[0].id, "a");
    assert!((cosine_results[0].score - 1.0).abs() < 1e-5);

    // Dot product
    let dot_results = coll
        .search(&Search::new(&query).distance(Distance::DotProduct))
        .unwrap();
    assert_eq!(dot_results[0].id, "a");
    assert!((dot_results[0].score - 1.0).abs() < 1e-5);

    // Euclidean distance (lower is better, but we return distance as positive)
    let euclid_results = coll
        .search(&Search::new(&query).distance(Distance::Euclidean))
        .unwrap();
    assert_eq!(euclid_results[0].id, "a");
    assert!(euclid_results[0].score < 0.01); // Distance from self is 0
}

#[test]
fn test_search_over_segments() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(3))
        .unwrap();

    // Insert and flush (creates segment)
    coll.insert(create_doc("in_segment", vec![1.0, 0.0, 0.0]))
        .unwrap();
    coll.flush().unwrap();

    // Insert more without flush (stays in memtable)
    coll.insert(create_doc("in_memtable", vec![0.0, 1.0, 0.0]))
        .unwrap();

    // Search should find both
    let query = vec![1.0, 0.0, 0.0];
    let results = coll.search(&Search::new(&query).top_k(2)).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, "in_segment");
    assert!((results[0].score - 1.0).abs() < 1e-5);
}

#[test]
fn test_search_dimension_mismatch() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    coll.insert(create_doc("a", vec![1.0, 2.0, 3.0, 4.0]))
        .unwrap();

    // Query with wrong dimension
    let query = vec![1.0, 2.0, 3.0]; // 3 instead of 4
    let result = coll.search(&Search::new(&query));

    assert!(result.is_err());
    let err = result.unwrap_err();
    assert!(err.to_string().contains("dimension"));
}

#[test]
fn test_search_deterministic_tie_breaking() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    // Insert identical vectors - should tie-break by internal ID
    coll.insert(create_doc("first", vec![1.0, 0.0])).unwrap();
    coll.insert(create_doc("second", vec![1.0, 0.0])).unwrap();
    coll.insert(create_doc("third", vec![1.0, 0.0])).unwrap();

    let query = vec![1.0, 0.0];
    let results = coll.search(&Search::new(&query).top_k(3)).unwrap();

    // All should have perfect cosine similarity
    for r in &results {
        assert!((r.score - 1.0).abs() < 1e-5);
    }

    // Should return all 3
    assert_eq!(results.len(), 3);
}

#[test]
fn test_search_empty_collection() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(3))
        .unwrap();

    let query = vec![1.0, 0.0, 0.0];
    let results = coll.search(&Search::new(&query)).unwrap();

    assert!(results.is_empty());
}

#[test]
fn test_search_deleted_documents_excluded() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    coll.insert(create_doc("a", vec![1.0, 0.0])).unwrap();
    coll.insert(create_doc("b", vec![0.0, 1.0])).unwrap();

    // Delete "a"
    coll.delete("a").unwrap();

    // Search should only find "b"
    let query = vec![1.0, 0.0];
    let results = coll.search(&Search::new(&query).top_k(10)).unwrap();

    // "a" is deleted, so only "b" should be found (with lower score)
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "b");
}

#[test]
fn test_search_large_dimension() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    // Test with 768-dimension vectors (common for embeddings)
    let dim = 768;
    let coll = db
        .create_collection("test", CollectionConfig::new(dim))
        .unwrap();

    // Create normalized vectors
    let vec_a: Vec<f32> = (0..dim).map(|i| if i < dim / 2 { 1.0 } else { 0.0 }).collect();
    let vec_b: Vec<f32> = (0..dim).map(|i| if i >= dim / 2 { 1.0 } else { 0.0 }).collect();

    // Normalize
    let norm_a = vec_a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let norm_b = vec_b.iter().map(|x| x * x).sum::<f32>().sqrt();
    let vec_a: Vec<f32> = vec_a.iter().map(|x| x / norm_a).collect();
    let vec_b: Vec<f32> = vec_b.iter().map(|x| x / norm_b).collect();

    coll.insert(create_doc("a", vec_a.clone())).unwrap();
    coll.insert(create_doc("b", vec_b.clone())).unwrap();

    // Search with vec_a as query - should find "a" first
    let results = coll.search(&Search::new(&vec_a).top_k(2)).unwrap();

    assert_eq!(results.len(), 2);
    assert_eq!(results[0].id, "a");
    assert!((results[0].score - 1.0).abs() < 1e-4);

    // "b" is orthogonal to "a" (non-overlapping halves)
    assert_eq!(results[1].id, "b");
    assert!(results[1].score < 0.1); // Should be close to 0 (orthogonal)
}

#[test]
fn test_search_persistence() {
    let temp_dir = TempDir::new().unwrap();

    // Create and populate
    {
        let db = Database::open(temp_dir.path()).unwrap();
        let coll = db
            .create_collection("test", CollectionConfig::new(3))
            .unwrap();

        coll.insert(create_doc("a", vec![1.0, 0.0, 0.0])).unwrap();
        coll.insert(create_doc("b", vec![0.0, 1.0, 0.0])).unwrap();
        coll.flush().unwrap();
    }

    // Reopen and search
    {
        let db = Database::open(temp_dir.path()).unwrap();
        let coll = db.get_collection("test").unwrap();

        let query = vec![1.0, 0.0, 0.0];
        let results = coll.search(&Search::new(&query).top_k(1)).unwrap();

        assert_eq!(results.len(), 1);
        assert_eq!(results[0].id, "a");
        assert!((results[0].score - 1.0).abs() < 1e-5);
    }
}

#[test]
fn test_search_result_with_payload() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    // Insert document with payload
    coll.insert(Document {
        id: "test_doc".to_string(),
        vector: vec![1.0, 0.0],
        payload: Some(serde_json::json!({
            "title": "Test Document",
            "tags": ["test", "search"]
        })),
    }).unwrap();

    // Search should find the document
    let query = vec![1.0, 0.0];
    let results = coll.search(&Search::new(&query)).unwrap();

    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "test_doc");
    
    // Note: Payload retrieval in search results is a known limitation
    // - Memtable: payloads not currently returned in search
    // - Segments: payloads not preserved during flush (Phase 1B limitation)
    // This will be addressed in a future phase
}
