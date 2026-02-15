//! RAG (Retrieval-Augmented Generation) example
//! 
//! This example shows how to build a simple RAG system
//! that stores document embeddings and retrieves relevant
//! context for LLM prompts.

use ndb::{Database, CollectionConfig, Document, Search, Filter, Durability};

/// A simple RAG system implementation
pub struct SimpleRAG {
    db: Database,
}

impl SimpleRAG {
    /// Create a new RAG system
    pub fn new(data_dir: &str) -> Result<Self, Box<dyn std::error::Error>> {
        let db = Database::open(data_dir)?;
        
        // Create collection for document chunks if it doesn't exist
        if !db.has_collection("chunks")? {
            // Use 1536 dimensions for OpenAI embeddings
            db.create_collection(
                "chunks",
                CollectionConfig::new(1536)
                    .with_durability(Durability::FdatasyncEachBatch),
            )?;
            println!("✓ Created 'chunks' collection");
        }
        
        Ok(Self { db })
    }
    
    /// Add a document chunk to the knowledge base
    /// 
    /// In a real application, you would:
    /// 1. Split documents into chunks
    /// 2. Generate embeddings using an LLM API
    /// 3. Store the chunks with metadata
    pub fn add_chunk(
        &self,
        chunk_id: &str,
        text: &str,
        embedding: Vec<f32>,
        source: &str,
    ) -> Result<(), ndb::Error> {
        let collection = self.db.get_collection("chunks")?;
        
        collection.insert(Document {
            id: chunk_id.to_string(),
            vector: embedding,
            payload: Some(serde_json::json!({
                "text": text,
                "source": source,
                "timestamp": chrono::Utc::now().to_rfc3339(),
            })),
        })?;
        
        Ok(())
    }
    
    /// Retrieve relevant context for a query
    /// 
    /// Returns the top-k most similar chunks to the query embedding
    pub fn retrieve_context(
        &self,
        query_embedding: &[f32],
        top_k: usize,
        source_filter: Option<&str>,
    ) -> Result<Vec<ContextChunk>, ndb::Error> {
        let collection = self.db.get_collection("chunks")?;
        
        // Build search with optional filter
        let mut search = Search::new(query_embedding)
            .top_k(top_k)
            .approximate(true)
            .ef(128);
        
        if let Some(source) = source_filter {
            search = search.filter(Filter::eq("source", source));
        }
        
        let results = collection.search(&search)?;
        
        Ok(results
            .into_iter()
            .map(|m| ContextChunk {
                chunk_id: m.id,
                text: m
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("text"))
                    .and_then(|t| t.as_str())
                    .unwrap_or("")
                    .to_string(),
                source: m
                    .payload
                    .as_ref()
                    .and_then(|p| p.get("source"))
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .to_string(),
                relevance_score: m.score,
            })
            .collect())
    }
    
    /// Build HNSW index for faster retrieval
    pub fn build_index(&self) -> Result<(), ndb::Error> {
        let collection = self.db.get_collection("chunks")?;
        collection.rebuild_index()?;
        println!("✓ Built HNSW index");
        Ok(())
    }
    
    /// Get statistics about the knowledge base
    pub fn stats(&self) -> Result<KnowledgeBaseStats, ndb::Error> {
        // This is a simplified example - real implementation would
        // query actual statistics from the database
        Ok(KnowledgeBaseStats {
            total_chunks: 0, // Would get from collection
            sources: vec![],
        })
    }
}

/// A context chunk retrieved for RAG
pub struct ContextChunk {
    pub chunk_id: String,
    pub text: String,
    pub source: String,
    pub relevance_score: f32,
}

/// Statistics about the knowledge base
pub struct KnowledgeBaseStats {
    pub total_chunks: usize,
    pub sources: Vec<String>,
}

fn main() -> Result<(), Box<dyn std::error::Error>> {
    println!("=== nDB RAG System Example ===\n");
    
    // Initialize RAG system
    let rag = SimpleRAG::new("./rag_data")?;
    println!("✓ RAG system initialized\n");
    
    // Example: Add some document chunks
    // In a real app, these would come from your documents
    // and be embedded using an LLM API
    
    println!("Adding document chunks...");
    
    // Note: Using dummy embeddings for demo (all zeros except first few dims)
    // In production, use actual embeddings from OpenAI, Hugging Face, etc.
    let example_embedding = vec![0.1; 1536];
    
    rag.add_chunk(
        "chunk_001",
        "Rust is a systems programming language that runs blazingly fast...",
        example_embedding.clone(),
        "rust_guide.md",
    )?;
    
    rag.add_chunk(
        "chunk_002",
        "Vector databases are essential for modern AI applications...",
        example_embedding.clone(),
        "vector_db_intro.md",
    )?;
    
    println!("✓ Added 2 chunks\n");
    
    // Build index for faster search
    rag.build_index()?;
    
    // Example: Retrieve context
    println!("Retrieving context for query...");
    let query_embedding = vec![0.1; 1536]; // Would be actual query embedding
    let context = rag.retrieve_context(&query_embedding, 3, None)?;
    
    println!("Retrieved {} chunks:", context.len());
    for chunk in &context {
        println!("  - {} (score: {:.4})", chunk.chunk_id, chunk.relevance_score);
        println!("    Source: {}", chunk.source);
        println!("    Text: {}...", &chunk.text[..chunk.text.len().min(50)]);
    }
    
    println!("\n✅ RAG example completed!");
    println!("\nIn a real application:");
    println!("  1. Use an embedding API (OpenAI, Hugging Face, etc.)");
    println!("  2. Chunk your documents appropriately");
    println!("  3. Retrieve context and include it in LLM prompts");
    
    Ok(())
}
