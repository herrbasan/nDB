//! Basic nDB usage example
//! 
//! Run with: cargo run --example basic_usage

use ndb::{Database, CollectionConfig, Document, Search, Filter};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Open or create a database
    let db = Database::open("./example_data")?;
    
    // Create a collection for 4-dimensional vectors (for demo purposes)
    let config = CollectionConfig::new(4);
    let collection = db.create_collection("items", config)?;
    
    println!("✓ Database and collection created");
    
    // Insert some documents
    let docs = vec![
        Document {
            id: "item_1".to_string(),
            vector: vec![1.0, 0.0, 0.0, 0.0],
            payload: Some(serde_json::json!({
                "name": "Red Item",
                "category": "electronics",
                "price": 100
            })),
        },
        Document {
            id: "item_2".to_string(),
            vector: vec![0.0, 1.0, 0.0, 0.0],
            payload: Some(serde_json::json!({
                "name": "Green Item",
                "category": "clothing",
                "price": 50
            })),
        },
        Document {
            id: "item_3".to_string(),
            vector: vec![0.0, 0.0, 1.0, 0.0],
            payload: Some(serde_json::json!({
                "name": "Blue Item",
                "category": "electronics",
                "price": 200
            })),
        },
    ];
    
    collection.insert_batch(docs)?;
    println!("✓ Inserted 3 documents");
    
    // Search for similar vectors
    let query = vec![0.9, 0.1, 0.0, 0.0];  // Close to item_1
    let search = Search::new(&query).top_k(2);
    let results = collection.search(&search)?;
    
    println!("\nSearch results for query [0.9, 0.1, 0.0, 0.0]:");
    for m in &results {
        println!("  {}: score={:.4}", m.id, m.score);
    }
    
    // Search with filter
    let filtered_search = Search::new(&query)
        .top_k(10)
        .filter(Filter::eq("category", "electronics"));
    let filtered_results = collection.search(&filtered_search)?;
    
    println!("\nFiltered search (electronics only):");
    for m in &filtered_results {
        println!("  {}: score={:.4}", m.id, m.score);
    }
    
    // Get a specific document
    if let Some(doc) = collection.get("item_1")? {
        println!("\nRetrieved item_1:");
        println!("  Vector: {:?}", doc.vector);
        println!("  Payload: {:?}", doc.payload);
    }
    
    // Delete a document
    collection.delete("item_2")?;
    println!("\n✓ Deleted item_2");
    
    // Compact to remove deleted documents
    let result = collection.compact()?;
    println!("✓ Compacted collection: {} → {} docs", result.docs_before, result.docs_after);
    
    println!("\n✅ Example completed successfully!");
    Ok(())
}
