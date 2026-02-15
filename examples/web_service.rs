//! Web service example using Axum
//! 
//! Run with: cargo run --example web_service --features axum
//! Test with: curl -X POST http://localhost:3000/search \
//!   -H "Content-Type: application/json" \
//!   -d '{"collection":"products","vector":[0.1,0.2,0.3,0.4],"top_k":5}'

 use std::sync::Arc;
// Note: This example requires the 'axum' and 'tokio' crates
// Add to Cargo.toml:
// [dependencies]
// axum = "0.7"
// tokio = { version = "1", features = ["full"] }
// serde = { version = "1.0", features = ["derive"] }

use ndb::{Database, CollectionConfig, Document, Search, Match};

/*
use axum::{
    extract::State,
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use serde::{Deserialize, Serialize};

struct AppState {
    db: Arc<Database>,
}

#[derive(Deserialize)]
struct SearchRequest {
    collection: String,
    vector: Vec<f32>,
    #[serde(default = "default_top_k")]
    top_k: usize,
}

#[derive(Deserialize)]
struct InsertRequest {
    collection: String,
    id: String,
    vector: Vec<f32>,
    payload: Option<serde_json::Value>,
}

#[derive(Serialize)]
struct SearchResponse {
    results: Vec<MatchResponse>,
}

#[derive(Serialize)]
struct MatchResponse {
    id: String,
    score: f32,
    payload: Option<serde_json::Value>,
}

fn default_top_k() -> usize {
    10
}

async fn search_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<SearchRequest>,
) -> Result<Json<SearchResponse>, (StatusCode, String)> {
    let collection = state
        .db
        .get_collection(&request.collection)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    
    let search = Search::new(&request.vector).top_k(request.top_k);
    let results = collection
        .search(&search)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    
    Ok(Json(SearchResponse {
        results: results
            .into_iter()
            .map(|m| MatchResponse {
                id: m.id,
                score: m.score,
                payload: m.payload,
            })
            .collect(),
    }))
}

async fn insert_handler(
    State(state): State<Arc<AppState>>,
    Json(request): Json<InsertRequest>,
) -> Result<StatusCode, (StatusCode, String)> {
    let collection = state
        .db
        .get_collection(&request.collection)
        .map_err(|e| (StatusCode::NOT_FOUND, e.to_string()))?;
    
    let doc = Document {
        id: request.id,
        vector: request.vector,
        payload: request.payload,
    };
    
    collection
        .insert(doc)
        .map_err(|e| (StatusCode::INTERNAL_SERVER_ERROR, e.to_string()))?;
    
    Ok(StatusCode::CREATED)
}

async fn health_handler(State(state): State<Arc<AppState>>) -> impl IntoResponse {
    // Simple health check - try to list collections
    match state.db.collection_names() {
        Ok(_) => (StatusCode::OK, "healthy"),
        Err(_) => (StatusCode::SERVICE_UNAVAILABLE, "unhealthy"),
    }
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    // Initialize database
    let db = Arc::new(Database::open("./web_service_data")?);
    
    // Create default collection if it doesn't exist
    if !db.has_collection("products")? {
        db.create_collection("products", CollectionConfig::new(768))?;
    }
    
    let state = Arc::new(AppState { db });
    
    // Build router
    let app = Router::new()
        .route("/health", get(health_handler))
        .route("/search", post(search_handler))
        .route("/insert", post(insert_handler))
        .with_state(state);
    
    println!("Server running on http://localhost:3000");
    println!("Endpoints:");
    println!("  POST /search  - Search for similar vectors");
    println!("  POST /insert  - Insert a new vector");
    println!("  GET  /health  - Health check");
    
    let listener = tokio::net::TcpListener::bind("0.0.0.0:3000").await?;
    axum::serve(listener, app).await?;
    
    Ok(())
}
*/

fn main() {
    println!("This example requires axum and tokio crates.");
    println!("Uncomment the code and add dependencies to run.");
    println!();
    println!("Example curl commands:");
    println!("  Search:  curl -X POST http://localhost:3000/search \\");
    println!("           -H 'Content-Type: application/json' \\");
    println!("           -d '{{\"collection\":\"products\",\"vector\":[0.1,0.2,0.3,0.4],\"top_k\":5}}'");
    println!();
    println!("  Insert:  curl -X POST http://localhost:3000/insert \\");
    println!("           -H 'Content-Type: application/json' \\");
    println!("           -d '{{\"collection\":\"products\",\"id\":\"item1\",\"vector\":[0.1,0.2,0.3,0.4]}}'");
}
