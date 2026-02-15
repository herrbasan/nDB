//! Phase 4: Filter DSL Integration Tests
//!
//! Tests for metadata filtering with Mongo-like filter DSL.

use ndb::{CollectionConfig, Database, Distance, Document, Filter, Search};
use tempfile::TempDir;

fn create_doc_with_payload(id: &str, vector: Vec<f32>, payload: serde_json::Value) -> Document {
    Document {
        id: id.to_string(),
        vector,
        payload: Some(payload),
    }
}

#[test]
fn test_filter_with_exact_search() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(3))
        .unwrap();

    // Insert documents with different categories
    coll.insert(create_doc_with_payload(
        "doc1",
        vec![1.0, 0.0, 0.0],
        serde_json::json!({"category": "books", "year": 2021}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "doc2",
        vec![0.0, 1.0, 0.0],
        serde_json::json!({"category": "movies", "year": 2022}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "doc3",
        vec![0.0, 0.0, 1.0],
        serde_json::json!({"category": "books", "year": 2020}),
    ))
    .unwrap();

    // Search with filter for "books" category
    let query = vec![1.0, 0.0, 0.0];
    let results = coll
        .search(&Search::new(&query).top_k(10).filter(Filter::eq("category", "books")))
        .unwrap();

    // Should only return doc1 and doc3
    assert_eq!(results.len(), 2);
    let ids: Vec<_> = results.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"doc1"));
    assert!(ids.contains(&"doc3"));
    assert!(!ids.contains(&"doc2"));
}

#[test]
fn test_filter_comparison_operators() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    // Insert documents with different scores
    coll.insert(create_doc_with_payload(
        "low",
        vec![1.0, 0.0],
        serde_json::json!({"score": 3.5}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "medium",
        vec![1.0, 0.0],
        serde_json::json!({"score": 7.0}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "high",
        vec![1.0, 0.0],
        serde_json::json!({"score": 9.5}),
    ))
    .unwrap();

    let query = vec![1.0, 0.0];

    // Test greater than
    let results = coll
        .search(&Search::new(&query).filter(Filter::gt("score", 5.0)))
        .unwrap();
    assert_eq!(results.len(), 2);
    let ids: Vec<_> = results.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"medium"));
    assert!(ids.contains(&"high"));

    // Test greater than or equal
    let results = coll
        .search(&Search::new(&query).filter(Filter::gte("score", 7.0)))
        .unwrap();
    assert_eq!(results.len(), 2);

    // Test less than
    let results = coll
        .search(&Search::new(&query).filter(Filter::lt("score", 7.0)))
        .unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "low");

    // Test less than or equal
    let results = coll
        .search(&Search::new(&query).filter(Filter::lte("score", 7.0)))
        .unwrap();
    assert_eq!(results.len(), 2);
}

#[test]
fn test_filter_and_or_operators() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    // Insert documents with multiple fields
    coll.insert(create_doc_with_payload(
        "book_2021",
        vec![1.0, 0.0],
        serde_json::json!({"category": "books", "year": 2021}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "book_2019",
        vec![1.0, 0.0],
        serde_json::json!({"category": "books", "year": 2019}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "movie_2021",
        vec![1.0, 0.0],
        serde_json::json!({"category": "movies", "year": 2021}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "movie_2019",
        vec![1.0, 0.0],
        serde_json::json!({"category": "movies", "year": 2019}),
    ))
    .unwrap();

    let query = vec![1.0, 0.0];

    // Test AND: books from 2021
    let filter = Filter::and([
        Filter::eq("category", "books"),
        Filter::gt("year", 2020),
    ]);
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "book_2021");

    // Test OR: books OR from 2021
    let filter = Filter::or([
        Filter::eq("category", "books"),
        Filter::gt("year", 2020),
    ]);
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 3); // book_2021, book_2019, movie_2021
}

#[test]
fn test_filter_in_operator() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    coll.insert(create_doc_with_payload(
        "a",
        vec![1.0, 0.0],
        serde_json::json!({"status": "active"}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "b",
        vec![1.0, 0.0],
        serde_json::json!({"status": "pending"}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "c",
        vec![1.0, 0.0],
        serde_json::json!({"status": "inactive"}),
    ))
    .unwrap();

    let query = vec![1.0, 0.0];

    // Test IN operator
    let filter = Filter::in_("status", ["active", "pending"]);
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 2);
    let ids: Vec<_> = results.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"b"));
    assert!(!ids.contains(&"c"));
}

#[test]
fn test_filter_nested_fields() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    coll.insert(create_doc_with_payload(
        "alice",
        vec![1.0, 0.0],
        serde_json::json!({
            "user": {
                "name": "alice",
                "age": 30
            }
        }),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "bob",
        vec![1.0, 0.0],
        serde_json::json!({
            "user": {
                "name": "bob",
                "age": 25
            }
        }),
    ))
    .unwrap();

    let query = vec![1.0, 0.0];

    // Test nested field access
    let filter = Filter::eq("user.name", "alice");
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "alice");

    // Test nested comparison
    let filter = Filter::gt("user.age", 25);
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "alice");
}

#[test]
fn test_filter_selective_returns_fewer_results() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    // Insert 10 documents with varying vectors (using dot product for clearer ranking)
    // Only 2 with "premium" status
    for i in 0..10 {
        let status = if i < 2 { "premium" } else { "standard" };
        coll.insert(create_doc_with_payload(
            &format!("doc{}", i),
            vec![i as f32, 10.0 - i as f32], // Varying vectors
            serde_json::json!({"status": status}),
        ))
        .unwrap();
    }

    let query = vec![5.0, 5.0]; // Query that will have varying similarity

    // Without filter: should return 5 documents
    let results = coll.search(&Search::new(&query).top_k(5)).unwrap();
    assert_eq!(results.len(), 5);

    // With filter for premium: should only get 2 results (doc0 and doc1)
    let results = coll
        .search(&Search::new(&query).top_k(5).filter(Filter::eq("status", "premium")))
        .unwrap();
    assert_eq!(results.len(), 2, "Expected 2 premium results"); // Fewer than top_k
    let ids: Vec<_> = results.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"doc0"), "Expected doc0 (premium)");
    assert!(ids.contains(&"doc1"), "Expected doc1 (premium)");
    // Verify no standard documents are returned
    assert!(!ids.contains(&"doc9"), "doc9 should be excluded (standard)");
}

#[test]
fn test_filter_no_matches() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    coll.insert(create_doc_with_payload(
        "doc1",
        vec![1.0, 0.0],
        serde_json::json!({"category": "books"}),
    ))
    .unwrap();

    let query = vec![1.0, 0.0];

    // Filter that matches nothing
    let filter = Filter::eq("category", "nonexistent");
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert!(results.is_empty());
}

#[test]
fn test_filter_over_segments() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    // Insert some documents and flush to segment
    coll.insert(create_doc_with_payload(
        "in_segment",
        vec![1.0, 0.0],
        serde_json::json!({"category": "books", "location": "segment"}),
    ))
    .unwrap();
    coll.flush().unwrap();

    // Insert more documents (stay in memtable)
    coll.insert(create_doc_with_payload(
        "in_memtable",
        vec![1.0, 0.0],
        serde_json::json!({"category": "books", "location": "memtable"}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "other",
        vec![1.0, 0.0],
        serde_json::json!({"category": "movies"}),
    ))
    .unwrap();

    let query = vec![1.0, 0.0];

    // Filter should work across both segment and memtable
    let filter = Filter::eq("category", "books");
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();

    assert_eq!(results.len(), 2);
    let ids: Vec<_> = results.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"in_segment"));
    assert!(ids.contains(&"in_memtable"));
}

#[test]
fn test_filter_with_approximate_search() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(4))
        .unwrap();

    // Insert documents with categories and flush to segments
    // (HNSW index in Phase 3 is designed to work with segment data)
    for i in 0..50 {
        let category = if i % 2 == 0 { "even" } else { "odd" };
        coll.insert(create_doc_with_payload(
            &format!("doc{}", i),
            vec![i as f32, (i * 2) as f32, (i * 3) as f32, (i * 4) as f32],
            serde_json::json!({"category": category}),
        ))
        .unwrap();
    }

    // Flush to create segments
    coll.flush().unwrap();

    // Build HNSW index from segments
    coll.rebuild_index().unwrap();
    assert!(coll.has_index(), "Index should be built");

    let query = vec![25.0, 50.0, 75.0, 100.0];

    // First verify approximate search works without filter
    let all_results = coll
        .search(&Search::new(&query).top_k(10).approximate(true))
        .unwrap();
    assert!(!all_results.is_empty(), "Approximate search should return results, got empty");

    // Approximate search with filter for "even" category
    let results = coll
        .search(
            &Search::new(&query)
                .top_k(10)
                .approximate(true)
                .filter(Filter::eq("category", "even")),
        )
        .unwrap();

    // All results should be "even" category (if any results returned)
    for result in &results {
        let payload = result.payload.as_ref().expect("Result should have payload");
        assert_eq!(payload["category"], "even", "Result should have even category: got {:?}", result);
    }
}

#[test]
fn test_filter_complex_nested() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    coll.insert(create_doc_with_payload(
        "match1",
        vec![1.0, 0.0],
        serde_json::json!({
            "product": {
                "category": "electronics",
                "details": {
                    "price": 999,
                    "in_stock": true
                }
            }
        }),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "match2",
        vec![1.0, 0.0],
        serde_json::json!({
            "product": {
                "category": "electronics",
                "details": {
                    "price": 499,
                    "in_stock": true
                }
            }
        }),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "no_match",
        vec![1.0, 0.0],
        serde_json::json!({
            "product": {
                "category": "clothing",
                "details": {
                    "price": 99,
                    "in_stock": true
                }
            }
        }),
    ))
    .unwrap();

    let query = vec![1.0, 0.0];

    // Complex filter: electronics AND price > 500
    let filter = Filter::and([
        Filter::eq("product.category", "electronics"),
        Filter::gt("product.details.price", 500),
    ]);

    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "match1");
}

#[test]
fn test_filter_numeric_type_coercion() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    // Document with integer value
    coll.insert(create_doc_with_payload(
        "doc",
        vec![1.0, 0.0],
        serde_json::json!({"count": 5}),
    ))
    .unwrap();

    let query = vec![1.0, 0.0];

    // Filter with float should match integer value
    let filter = Filter::eq("count", 5.0);
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 1);

    // Filter with integer should also work
    let filter = Filter::eq("count", 5);
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 1);
}

#[test]
fn test_filter_documents_without_payload() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(2))
        .unwrap();

    // Document with payload
    coll.insert(create_doc_with_payload(
        "with_payload",
        vec![1.0, 0.0],
        serde_json::json!({"category": "books"}),
    ))
    .unwrap();

    // Document without payload
    coll.insert(Document {
        id: "no_payload".to_string(),
        vector: vec![1.0, 0.0],
        payload: None,
    })
    .unwrap();

    let query = vec![1.0, 0.0];

    // Filter should exclude documents without payload
    let filter = Filter::eq("category", "books");
    let results = coll.search(&Search::new(&query).filter(filter)).unwrap();
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].id, "with_payload");
}

#[test]
fn test_filter_combined_with_distance_metric() {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();

    let coll = db
        .create_collection("test", CollectionConfig::new(3))
        .unwrap();

    coll.insert(create_doc_with_payload(
        "a",
        vec![1.0, 0.0, 0.0],
        serde_json::json!({"active": true}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "b",
        vec![0.0, 1.0, 0.0],
        serde_json::json!({"active": false}),
    ))
    .unwrap();

    coll.insert(create_doc_with_payload(
        "c",
        vec![0.0, 0.0, 1.0],
        serde_json::json!({"active": true}),
    ))
    .unwrap();

    let query = vec![1.0, 0.0, 0.0];

    // Filter active documents with dot product distance
    let results = coll
        .search(
            &Search::new(&query)
                .distance(Distance::DotProduct)
                .filter(Filter::eq("active", true)),
        )
        .unwrap();

    assert_eq!(results.len(), 2);
    let ids: Vec<_> = results.iter().map(|m| m.id.as_str()).collect();
    assert!(ids.contains(&"a"));
    assert!(ids.contains(&"c"));
    assert!(!ids.contains(&"b"));

    // First result should be "a" (most similar to query)
    assert_eq!(results[0].id, "a");
}
