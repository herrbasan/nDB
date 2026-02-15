use criterion::{black_box, criterion_group, criterion_main, Criterion, BatchSize, BenchmarkId};
use ndb::{Database, CollectionConfig};
use serde_json::json;
use tempfile::TempDir;

fn generate_vector(dim: usize, seed: usize) -> Vec<f32> {
    (0..dim)
        .map(|j| ((seed * dim + j) % 100) as f32 / 100.0)
        .collect()
}

fn bench_insert_single(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_single");
    group.sample_size(100);
    
    group.bench_function("single_128d", |b| {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();
        let config = CollectionConfig::new(128);
        let collection = db.create_collection("test", config).unwrap();
        
        let mut i = 0usize;
        b.iter(|| {
            let vec = generate_vector(128, i);
            collection.insert(ndb::Document {
                id: format!("doc_{}", i),
                vector: vec,
                payload: Some(json!({"idx": i})),
            }).unwrap();
            i += 1;
        });
        
        drop(collection);
        drop(db);
    });
    
    group.finish();
}

fn bench_insert_batch(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_batch");
    group.sample_size(50);
    
    for batch_size in [10, 100, 1000].iter() {
        group.bench_with_input(
            BenchmarkId::new("batch_128d", batch_size),
            batch_size,
            |b, &batch_size| {
                b.iter_batched(
                    || {
                        // Setup: create temp dir and prepare documents
                        let temp_dir = TempDir::new().unwrap();
                        let db = Database::open(temp_dir.path()).unwrap();
                        let config = CollectionConfig::new(128);
                        let collection = db.create_collection("test", config).unwrap();
                        
                        let docs: Vec<_> = (0..batch_size)
                            .map(|i| ndb::Document {
                                id: format!("doc_{}", i),
                                vector: generate_vector(128, i),
                                payload: Some(json!({"idx": i})),
                            })
                            .collect();
                        
                        (temp_dir, db, collection, docs)
                    },
                    |(_temp_dir, _db, collection, docs)| {
                        // Benchmark the batch insert
                        black_box(collection.insert_batch(docs).unwrap());
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    
    group.finish();
}

fn bench_insert_throughput(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_throughput");
    group.throughput(criterion::Throughput::Elements(1000));
    group.sample_size(20);
    
    group.bench_function("docs_per_sec", |b| {
        let temp_dir = TempDir::new().unwrap();
        let db = Database::open(temp_dir.path()).unwrap();
        let config = CollectionConfig::new(128);
        let collection = db.create_collection("test", config).unwrap();
        
        let docs: Vec<_> = (0..1000)
            .map(|i| ndb::Document {
                id: format!("doc_{}", i),
                vector: generate_vector(128, i),
                payload: Some(json!({"idx": i})),
            })
            .collect();
        
        b.iter(|| {
            collection.insert_batch(black_box(docs.clone())).unwrap();
        });
        
        drop(collection);
        drop(db);
    });
    
    group.finish();
}

fn bench_insert_dimensions(c: &mut Criterion) {
    let mut group = c.benchmark_group("insert_by_dimension");
    group.sample_size(50);
    
    for dim in [384, 768, 1536].iter() {
        group.bench_with_input(BenchmarkId::new("dim", dim), dim, |b, &dim| {
            let temp_dir = TempDir::new().unwrap();
            let db = Database::open(temp_dir.path()).unwrap();
            let config = CollectionConfig::new(dim);
            let collection = db.create_collection("test", config).unwrap();
            
            let mut i = 0usize;
            b.iter(|| {
                let vec = generate_vector(dim, i);
                collection.insert(ndb::Document {
                    id: format!("doc_{}", i),
                    vector: vec,
                    payload: None,
                }).unwrap();
                i += 1;
            });
            
            drop(collection);
            drop(db);
        });
    }
    
    group.finish();
}

criterion_group!(
    benches,
    bench_insert_single,
    bench_insert_batch,
    bench_insert_throughput,
    bench_insert_dimensions
);
criterion_main!(benches);
