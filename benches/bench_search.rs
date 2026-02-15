use criterion::{black_box, criterion_group, criterion_main, Criterion, BenchmarkId};
use ndb::{Database, CollectionConfig, Distance, Search};
use serde_json::json;
use tempfile::TempDir;

fn generate_vectors(count: usize, dim: usize) -> Vec<Vec<f32>> {
    (0..count)
        .map(|i| {
            (0..dim)
                .map(|j| ((i * dim + j) % 100) as f32 / 100.0)
                .collect()
        })
        .collect()
}

fn bench_exact_search(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();
    let config = CollectionConfig::new(128);
    let collection = db.create_collection("test", config).unwrap();
    
    // Insert test data
    let vectors = generate_vectors(10000, 128);
    for (i, vec) in vectors.iter().enumerate() {
        collection.insert(ndb::Document {
            id: format!("doc_{}", i),
            vector: vec.clone(),
            payload: Some(json!({"idx": i})),
        }).unwrap();
    }
    
    let query = generate_vectors(1, 128)[0].clone();
    
    let mut group = c.benchmark_group("exact_search");
    group.sample_size(100);
    
    for size in [1000, 5000, 10000].iter() {
        group.bench_with_input(BenchmarkId::new("cosine", size), size, |b, &_size| {
            let search = Search::new(&query)
                .top_k(10)
                .approximate(false);
            b.iter(|| {
                black_box(collection.search(&search).unwrap());
            });
        });
    }
    
    group.finish();
    
    // Cleanup
    drop(collection);
    drop(db);
}

fn bench_hnsw_search(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();
    let config = CollectionConfig::new(128);
    let collection = db.create_collection("test", config).unwrap();
    
    // Insert test data
    let vectors = generate_vectors(10000, 128);
    for (i, vec) in vectors.iter().enumerate() {
        collection.insert(ndb::Document {
            id: format!("doc_{}", i),
            vector: vec.clone(),
            payload: Some(json!({"idx": i})),
        }).unwrap();
    }
    
    // Build HNSW index
    collection.rebuild_index().unwrap();
    
    let query = generate_vectors(1, 128)[0].clone();
    
    let mut group = c.benchmark_group("hnsw_search");
    group.sample_size(100);
    
    // Benchmark different ef values
    for ef in [32, 64, 128].iter() {
        group.bench_with_input(BenchmarkId::new("ef", ef), ef, |b, &ef| {
            let search = Search::new(&query)
                .top_k(10)
                .approximate(true)
                .ef(ef);
            b.iter(|| {
                black_box(collection.search(&search).unwrap());
            });
        });
    }
    
    // Benchmark different dataset sizes
    group.bench_function("hnsw_10k", |b| {
        let search = Search::new(&query)
            .top_k(10)
            .approximate(true)
            .ef(64);
        b.iter(|| {
            black_box(collection.search(&search).unwrap());
        });
    });
    
    group.finish();
    
    // Cleanup
    drop(collection);
    drop(db);
}

fn bench_filtered_search(c: &mut Criterion) {
    let temp_dir = TempDir::new().unwrap();
    let db = Database::open(temp_dir.path()).unwrap();
    let config = CollectionConfig::new(128);
    let collection = db.create_collection("test", config).unwrap();
    
    // Insert test data with payloads
    let vectors = generate_vectors(10000, 128);
    for (i, vec) in vectors.iter().enumerate() {
        collection.insert(ndb::Document {
            id: format!("doc_{}", i),
            vector: vec.clone(),
            payload: Some(json!({
                "category": if i % 10 == 0 { "special" } else { "normal" },
                "value": i % 100
            })),
        }).unwrap();
    }
    
    let query = generate_vectors(1, 128)[0].clone();
    let filter = ndb::Filter::eq("category", "special");
    
    let mut group = c.benchmark_group("filtered_search");
    group.sample_size(100);
    
    group.bench_function("exact_with_filter", |b| {
        let search = Search::new(&query)
            .top_k(10)
            .approximate(false)
            .filter(filter.clone());
        b.iter(|| {
            black_box(collection.search(&search).unwrap());
        });
    });
    
    group.finish();
    
    // Cleanup
    drop(collection);
    drop(db);
}

fn bench_search_dimensions(c: &mut Criterion) {
    let mut group = c.benchmark_group("search_by_dimension");
    group.sample_size(50);
    
    for dim in [384, 768, 1536].iter() {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();
        let config = CollectionConfig::new(*dim);
        let collection = db.create_collection("test", config).unwrap();
        
        // Insert test data
        let vectors = generate_vectors(1000, *dim);
        for (i, vec) in vectors.iter().enumerate() {
            collection.insert(ndb::Document {
                id: format!("doc_{}", i),
                vector: vec.clone(),
                payload: None,
            }).unwrap();
        }
        
        let query = generate_vectors(1, *dim)[0].clone();
        
        group.bench_with_input(BenchmarkId::new("dim", dim), dim, |b, &_dim| {
            let search = Search::new(&query)
                .top_k(10)
                .approximate(false);
            b.iter(|| {
                black_box(collection.search(&search).unwrap());
            });
        });
        
        drop(collection);
        drop(db);
    }
    
    group.finish();
}

criterion_group!(
    benches,
    bench_exact_search,
    bench_hnsw_search,
    bench_filtered_search,
    bench_search_dimensions
);
criterion_main!(benches);
