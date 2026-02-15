use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use ndb::{Database, CollectionConfig};
use serde_json::json;
use tempfile::TempDir;
use std::fs;

fn generate_vectors(count: usize, dim: usize) -> Vec<Vec<f32>> {
    (0..count)
        .map(|i| {
            (0..dim)
                .map(|j| ((i * dim + j) % 100) as f32 / 100.0)
                .collect()
        })
        .collect()
}

/// Create a database with a specified WAL size (approximate)
fn create_db_with_wal_size(doc_count: usize) -> (TempDir, String) {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();
    let config = CollectionConfig::new(128);
    let collection = db.create_collection("test", config).unwrap();
    
    // Insert documents (without flushing to keep them in WAL)
    let vectors = generate_vectors(doc_count, 128);
    for (i, vec) in vectors.iter().enumerate() {
        collection.insert(ndb::Document {
            id: format!("doc_{}", i),
            vector: vec.clone(),
            payload: Some(json!({"idx": i, "data": "some payload data to increase size"})),
        }).unwrap();
    }
    
    // Get the collection path
    let collection_path = temp_dir.path()
        .join("test")
        .to_string_lossy()
        .to_string();
    
    // Get WAL size before closing
    let wal_path = temp_dir.path().join("test").join("wal.log");
    let wal_size = if wal_path.exists() {
        fs::metadata(&wal_path).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    
    // Close the database
    drop(collection);
    drop(db);
    
    println!("Created DB with ~{} docs, WAL size: {} bytes", doc_count, wal_size);
    
    (temp_dir, collection_path)
}

fn bench_recovery_by_doc_count(c: &mut Criterion) {
    let mut group = c.benchmark_group("recovery_time");
    group.sample_size(20);
    
    // Pre-create databases of different sizes
    for doc_count in [1000, 5000, 10000].iter() {
        group.bench_with_input(
            BenchmarkId::new("docs", doc_count),
            doc_count,
            |b, &_count| {
                // Create fresh database for each iteration
                let (temp_dir, _collection_path) = create_db_with_wal_size(*doc_count);
                
                b.iter(|| {
                    // Reopen the database (triggers WAL replay)
                    let db = black_box(Database::open(temp_dir.path()).unwrap());
                    black_box(db);
                });
            },
        );
    }
    
    group.finish();
}

fn bench_recovery_with_segments(c: &mut Criterion) {
    let mut group = c.benchmark_group("recovery_with_segments");
    group.sample_size(10);
    
    group.bench_function("flushed_data", |b| {
        let temp_dir = TempDir::new().unwrap();
        
        // Create and populate database
        {
            let db = Database::open(temp_dir.path()).unwrap();
            let config = CollectionConfig::new(128);
            let collection = db.create_collection("test", config).unwrap();
            
            // Insert documents and flush to create segments
            let vectors = generate_vectors(10000, 128);
            for (i, vec) in vectors.iter().enumerate() {
                collection.insert(ndb::Document {
                    id: format!("doc_{}", i),
                    vector: vec.clone(),
                    payload: Some(json!({"idx": i})),
                }).unwrap();
            }
            
            // Flush to create segments
            collection.flush().unwrap();
            
            // Insert more documents (stay in WAL)
            for i in 10000..10500 {
                let vec = generate_vector(128, i);
                collection.insert(ndb::Document {
                    id: format!("doc_{}", i),
                    vector: vec,
                    payload: Some(json!({"idx": i})),
                }).unwrap();
            }
            
            drop(collection);
            drop(db);
        }
        
        b.iter(|| {
            // Reopen database with both segments and WAL
            let db = black_box(Database::open(temp_dir.path()).unwrap());
            black_box(db);
        });
    });
    
    group.finish();
}

fn bench_index_build_time(c: &mut Criterion) {
    let mut group = c.benchmark_group("index_build_time");
    group.sample_size(10);
    
    for doc_count in [1000, 5000, 10000].iter() {
        group.bench_with_input(
            BenchmarkId::new("hnsw", doc_count),
            doc_count,
            |b, &count| {
                let temp_dir = TempDir::new().unwrap();
                let db = Database::open(temp_dir.path()).unwrap();
                let config = CollectionConfig::new(128);
                let collection = db.create_collection("test", config).unwrap();
                
                // Insert documents
                let vectors = generate_vectors(count, 128);
                for (i, vec) in vectors.iter().enumerate() {
                    collection.insert(ndb::Document {
                        id: format!("doc_{}", i),
                        vector: vec.clone(),
                        payload: None,
                    }).unwrap();
                }
                
                b.iter(|| {
                    // Delete existing index if present
                    let _ = collection.delete_index();
                    // Build HNSW index
                    black_box(collection.rebuild_index().unwrap());
                });
                
                drop(collection);
                drop(db);
            },
        );
    }
    
    group.finish();
}

fn generate_vector(dim: usize, seed: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| ((seed * dim + j) % 100) as f32 / 100.0)
        .collect()
}

criterion_group!(
    benches,
    bench_recovery_by_doc_count,
    bench_recovery_with_segments,
    bench_index_build_time
);
criterion_main!(benches);
